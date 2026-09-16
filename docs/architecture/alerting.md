# 13. Alerting

**Built**: `crates/mira/src/alert.rs`. Static rules in a KYAML file named by
`alerts.rules`, evaluated on a timer, dispatched as JSON webhooks. Off unless
the file is named. The operator-facing half is [config.md](../config.md); this
section is why it has the shape it does.

## 13.1 A rule embeds a query document, verbatim

The `query` and `of` keys of a rule are `/api/v1/query` documents parsed by
`api::parse_search` — the same parser the HTTP API, the MCP tools, the browser
UI's filter bar and the TUI's all reach. There is no alert filter grammar.

That is the only way the link in a page can be trusted. The UI keeps every bit
of its view state in the location hash (section 8.2), so `link_base` plus the
rule's own terms *is* the saved view: no view has to be created, stored or
garbage-collected, and principle 4 survives an alerting engine intact. If a rule
spelled its predicate differently from the UI, the link in the 3am page would
open a different set of rows than the number that woke someone up, and nothing
would ever detect it. The rule's terms are re-spelled into the filter-bar
grammar once, in `filter_of`, and both the link and the `filter` field of
`/api/v1/alerts` come out of that one function — which lets the TUI's alert pane
press Enter and land on the rows.

A rule may not set `from`, `to`, `limit` or `after`. `over` is the window and
the evaluator owns the rest; those keys are refused at load rather than
overwritten in silence.

## 13.2 Counting is free, so there is no aggregation engine

`query::search_open` with `limit: 0` returns an exact `stats.rows_matched` and
no rows. The early exit cannot fire (it tests `hits.last()`, which is always
`None` after a zero-limit trim), the match counter accumulates every wave, and
memory is bounded because nothing is retained. So an evaluation is one scan for
a count rule and two for a ratio rule, over one window, with no rows
materialised and no new code in `mira-core`.

This is a property of the internal call, not the HTTP endpoint: `/api/v1/query`
still rejects `limit: 0`, because a caller asking for zero rows through the API
has almost certainly made a mistake.

## 13.3 A percentile threshold *is* a ratio threshold

```text
p95(d) > T   ⟺   |{ d > T }| / |d| > 0.05
```

Exactly, not approximately: both sides say "more than 5% of the sample exceeds
T". So the two metrics `count` and `ratio` cover every threshold in the original
brief, including `p95(duration) > 500ms`, and there is no t-digest, no sketch in
the block footer to maintain, and no `p95(...)` in the grammar. A sketch here
would be an approximation of something a scan answers exactly and for free
(section 13.2). `alert::tests::a_percentile_threshold_is_a_ratio_threshold` is the
identity as an executable claim.

The corollary: this buys thresholds, not *values*. Mira cannot currently tell
you what p95 *is*, only whether it is over a line you named. When "what is the
p95" becomes the question, the answer is the block-footer digest of section 8.1,
a read-path feature rather than an alerting one.

## 13.4 Which replica pages

Nothing elects one. Alerting is off unless a node is pointed at a rules file, so
in a fleet exactly one replica is given the flag and it is the one that pages.
That is the whole coordination mechanism, and it is deployment configuration
rather than state — three replicas sharing the file send three copies of every
alert, a misconfiguration and not a race.

`ponytail:` the upgrade path, if that ever stops being acceptable, is a lease
file in the block directory: an evaluator writes `alerts/lease` with its node id
and a timestamp, and refuses to evaluate if a fresher one exists. Still no
coordination *service* — the directory is already the manifest (section 3.2) —
but it is coordination state, so not worth paying for until someone is running
two evaluators by accident.

## 13.5 Dispatch, and why TLS is a Cargo feature

One POST per edge — `ok → firing` and `firing → ok` — with a 10-second timeout
and no retry. A retry queue is durable state on a node that is supposed to have
none, and the next evaluation is along in `every` seconds regardless. `for` is a
*sustained* breach, not a repeated one: the state machine (`State::advance`)
resets `since` on any non-breaching evaluation, so a metric that flaps across the
line never accumulates hold time. PagerDuty gets `dedup_key = rule name`, so a resolve
closes the incident its fire opened.

A direct `https://` target costs eleven crates and brings `ring`, which is C and
assembly. "The tree is N crates" and "`zstd-sys` is the only C dependency" are
both stated product properties (section 11, README), so HTTPS is `--features
webhook-tls`: 120 crates by default, 131 with it. Those two are `cargo tree`
counts including the three workspace members; the 122 in section 11 and the
README is the same tree without them. The default build refuses an `https://`
URL when the rules file is *loaded* — at boot, with the process exiting —
rather than at the first page, because the first page is when nobody is reading
logs. An egress proxy on localhost is the zero-crate answer.

Neither `Rules`, `Rule` nor `Target_` derives `Debug`. A Slack webhook URL *is*
the credential, and so is a PagerDuty routing key; a derive would put both one
careless `{:?}` away from a log line.

## 13.6 The surfaces

`GET /api/v1/alerts` is always routed, even with no rules — an empty list is how
the UI, the TUI and an agent learn that alerting is *off*, where a 404 is
indistinguishable from an old build. Every rule is reported, firing or not, with
its last value, both record counts, its `filter`, its `link`, and an `error`
that is non-null when the rule could not be evaluated: a rule that failed to
evaluate is not a rule that is quiet, and no surface here conflates them. The
MCP tool `list_alerts` and the TUI's `a` pane read the same document.
