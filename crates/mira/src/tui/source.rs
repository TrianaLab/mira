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

    /// The claim that pays for this whole module: a block directory answers with
    /// nothing running. No server, no port, no process that has to have survived
    /// — a detached PVC is still readable.
    ///
    /// All four route arms go through here because the field name each one puts
    /// in the envelope (`rows`, `series`, `names`) is what the TUI reads back
    /// out, and a route that answered under the wrong key would look like an
    /// empty result rather than an error.
    #[test]
    fn a_local_source_answers_out_of_a_directory_with_no_server() {
        let dir = std::env::temp_dir().join(format!("mira-src-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut b = mira_core::logs::LogsBuilder::new();
        b.append_request(&crate::e2e::logs_export("checkout", 1_000, 6))
            .unwrap();
        let sealed = b.finish().unwrap();
        mira_core::block::publish(&dir, "logs", mira_core::block::node_id("a"), 0, &sealed)
            .unwrap();

        let src = Source::Local(dir.clone());
        assert!(src.label().starts_with("local "));

        let d = src
            .post(QUERY, r#"{"signal":"logs","from":0,"to":100000,"limit":2}"#)
            .unwrap();
        assert_eq!(d["rows"].as_vec().unwrap().len(), 2);
        assert_eq!(d["stats"]["rows_matched"].as_i64().unwrap(), 6);

        // No metrics in this directory, so these answer empty — which is the
        // point: they answer, under their own key, rather than erroring.
        for (route, field) in [(SERIES, "series"), (NAMES, "names")] {
            let d = src.post(route, r#"{"name":"anything"}"#).unwrap();
            assert!(d[field].as_vec().unwrap().is_empty(), "{route}");
            assert_eq!(d["stats"]["blocks_total"].as_i64().unwrap(), 0);
        }

        // A malformed document is the engine's error, reported verbatim rather
        // than swallowed into "query failed".
        let e = src.post(QUERY, r#"{"signal":"nope"}"#).unwrap_err();
        assert!(e.contains("nope"), "{e}");
        assert!(
            src.post("/api/v1/nope", "{}")
                .unwrap_err()
                .contains("route")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Enough HTTP to read an answer, and no more — so what it does with the
    /// three replies it can get has to be pinned down here.
    ///
    /// A non-200 carrying a JSON body is handed up rather than reported as a
    /// number, because the body is the error envelope and the message inside it
    /// is the only useful thing on the screen.
    #[test]
    fn a_remote_source_reports_what_the_server_actually_said() {
        // One canned reply per connection, in order. The listener lives as long
        // as the thread, which ends when the replies run out.
        fn serve(replies: Vec<&'static str>) -> String {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = l.local_addr().unwrap().to_string();
            std::thread::spawn(move || {
                for reply in replies {
                    let Ok((mut s, _)) = l.accept() else { return };
                    // The whole request, head then body. Replying while the
                    // body is still unread closes the socket with data in the
                    // receive queue, which is an RST on the client rather than
                    // the answer — a real server has the same obligation.
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while std::io::Read::read(&mut s, &mut byte).unwrap_or(0) == 1 {
                        head.push(byte[0]);
                        if head.ends_with(b"\r\n\r\n") {
                            break;
                        }
                    }
                    let len: usize = String::from_utf8_lossy(&head)
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length: ")?.trim().parse().ok())
                        .unwrap_or(0);
                    let mut body = vec![0u8; len];
                    let _ = std::io::Read::read_exact(&mut s, &mut body);
                    let _ = s.write_all(reply.as_bytes());
                }
            });
            addr
        }

        let ok = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"rows\":[],\
                  \"stats\":{\"blocks_total\":0,\"blocks_scanned\":0,\"rows_scanned\":0,\
                  \"rows_matched\":0}}";
        let bad = "HTTP/1.1 400 Bad Request\r\n\r\n{\"error\":\"unknown signal \\\"nope\\\"\"}";
        let plain = "HTTP/1.1 502 Bad Gateway\r\n\r\nupstream is down";
        let truncated = "HTTP/1.1 200 OK\r\nContent-Type: application/json";

        let src = Source::Remote(serve(vec![ok, bad, plain, truncated]));
        assert!(src.label().starts_with("http "));

        let d = src.post(QUERY, "{}").unwrap();
        assert!(d["rows"].as_vec().unwrap().is_empty());
        // 400, but the message is what reaches the status bar, not the number.
        assert_eq!(
            src.post(QUERY, "{}").unwrap_err(),
            r#"unknown signal "nope""#
        );
        let e = src.post(QUERY, "{}").unwrap_err();
        assert!(e.contains("502") && e.contains("upstream is down"), "{e}");
        assert!(
            src.post(QUERY, "{}").unwrap_err().contains("terminator"),
            "a reply with no blank line is not an empty answer"
        );

        // Nothing listening at all. The address is in the message because the
        // usual cause is a typo in `--addr`.
        let dead = Source::Remote("127.0.0.1:1".into());
        let e = dead.post(QUERY, "{}").unwrap_err();
        assert!(e.starts_with("127.0.0.1:1: "), "{e}");
    }
}
