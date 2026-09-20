# Mira — Architecture

**For:** anyone about to change the engine, and anyone deciding whether its
trade-offs are the ones they want. Not a getting-started page — that is
[See it work](../demo.md).

**This document is the *why*.** The *what* is generated from the code and
published at **[miradb.dev/api](https://miradb.dev/api/)** — rustdoc over the
whole workspace including private items, because in a binary crate the
mechanisms are all private. A doc comment and this document can never disagree
about behaviour, because only one of them describes behaviour.

| If you are looking for | Read |
| --- | --- |
| the on-disk block format, publish and scan | [`mira_core::block`](https://miradb.dev/api/mira_core/block/index.html), and section 3 below for why it is shaped that way |
| the Arrow schemas, column by column | [`mira_core::schema`](https://miradb.dev/api/mira_core/schema/index.html) |
| filters, pruning and the query executor | [`mira_core::query`](https://miradb.dev/api/mira_core/query/index.html), [`attrs`](https://miradb.dev/api/mira_core/attrs/index.html), [`zone`](https://miradb.dev/api/mira_core/zone/index.html), [`bloom`](https://miradb.dev/api/mira_core/bloom/index.html) |
| the ingest channel, flusher and retention worker | [`mira::pipeline`](https://miradb.dev/api/mira/pipeline/index.html) |
| the write-ahead log | [`mira_core::wal`](https://miradb.dev/api/mira_core/wal/index.html) |
| every HTTP route and its body | [HTTP API](../reference/http.md) — generated from the router |
| every flag and config key | [CLI](../reference/cli.md), [Configuration](../config.md) — generated from the binary and from `Config` |

**Status:** the workspace under `crates/` implements most of this and its tests
pass. Sections marked "Not built" are the exceptions; 0.1 below is the
complete list, and the README's *Scope* is the three-line version of it.

## Reading order

Sections 0 and 1 first, and before changing anything structural: 0 lists the
mechanisms from the original brief that do not survive contact with the
formats, and re-proposing one of them is the most common way to waste a day.

| | |
| --- | --- |
| [0. Corrections to the original brief](corrections.md) | what was proposed, what the formats refused, and what is not true yet |
| [1. Principles](principles.md) | the five constraints, and the mechanism each one buys |
| [2. Workspace](workspace.md) | three crates, and why the split falls there |
| [3. Data layout](data-layout.md) | schemas, blocks, the directory that is the manifest |
| [4. Ingest path](ingest.md) | OTLP in, to a sealed block |
| [5. Flusher state machine](flushers.md) | when a block seals |
| [6. Retention worker](retention.md) | expiry, compaction, offload |
| [7. Correlation](correlation.md) | join keys, entity identity, the frame algebra |
| [8. Read surfaces](read-surfaces.md) | what an agent sees and what a human sees |
| [9. Durability and failure model](durability.md) | what a crash costs, and what it cannot cost |
| [10. What is deliberately not here](not-here.md) | the refusals, each with its reason |
| [11. Performance model](performance.md) | the corpora, the four axes, how to read the table |
| [… the ingest plateau](performance-ingest.md) | one mutex, and the two fixes that were priced and rejected |
| [… verification and restart](performance-durability.md) | what a query pays to trust a block, and what a restart replays |
| [… query cost](performance-query.md) | the block is the unit, and the two levers that moved it |
| [… cost per GB](performance-cost.md) | hot and compacted, and what compresses |
| [12. Multiple active replicas](replicas.md) | ingest without coordination, and the query proxy |
| [… scaling and the coordinator](replicas-scaling.md) | discovery, what scaling buys, and why the controller is a second binary |
| [13. Alerting](alerting.md) | the rules, and where they run |
| [14. Cluster context](kubernetes-context.md) | what Kubernetes knows about a pod, on the same timeline, as logs |
| [15. The write-up](rca.md) | `render_rca`, and the citations it re-runs before it renders |
