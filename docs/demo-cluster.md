---
description: The same demo with Kubernetes on the path — Kind, the operator, Envoy Gateway and an HTTPRoute — serving the UI, the query API and the MCP endpoint on http://localhost:8080.
---

# See it on Kubernetes

**For:** anyone who will run Mira in a cluster and wants to see the operator, the
proxy and a real ingress carrying it. One command, a few minutes, and it leaves
everything running.

```sh
make demo-cluster
```

It needs `docker`, `kind`, `kubectl` and `helm`.

[See it work](demo.md) is the same product without any of this, and it is
quicker. Start there if the question is what Mira does; this one answers what it
looks like once Kubernetes is in front of it.

## What it builds

A one-node [Kind](https://kind.sigs.k8s.io/) cluster with the node's 30080
forwarded to the host's 8080, then, in order: Envoy Gateway, the operator from
`charts/mira-operator`, a `Gateway`, a `MiraCluster` of one storage replica and
one proxy, `telemetrygen` exporting into the proxy for as long as the cluster
lives, and `cart`, a pod with a memory limit too low to survive. Last, it seeds
an hour of a four-service shop through the Gateway from the host — the same
generator `make demo` uses, so there is something worth asking questions about
before the live traffic has accumulated any.

Nothing has to stay in the foreground. `make demo-cluster-down` deletes the
cluster, which is all of it.

## The shape of it

```mermaid
flowchart LR
  B["browser<br/>agent · curl"] --> G

  subgraph host [" localhost:8080 "]
    G["Envoy Gateway<br/>HTTPRoute"]
  end

  G -- "/v1/* · /api/v1/query" --> P["tel-proxy"]
  G -- "everything else" --> N["tel-0<br/>UI · /mcp · blocks"]
  P --> N
  T["telemetrygen"] --> P
  O["mira-operator"] -. "owns" .-> P
  O -. "owns" .-> N
  O -. "writes the route" .-> G

  style G fill:#1f6feb,color:#fff
  style O fill:#30363d,color:#fff
```

The operator writes that `HTTPRoute`, from four lines of `spec.route` naming the
Gateway — see [the CRD reference](reference/crd.md). The split is not a demo
choice and not configurable: `/v1/*` is OTLP ingest and `/api/v1/query` is the
read a proxy can merge, so both go to the proxy, and the UI, `/mcp` and the
reads built by walking one node's blocks live on a storage node, so everything
else goes there.

That split is also why the demo runs one storage replica. With two, the browser
would be reading half the corpus while the query API read all of it.
`make operator-e2e` is where the multi-replica tier is exercised.

## What to do with it

| | |
| --- | --- |
| UI | `http://localhost:8080/` |
| Terminal UI | `target/release/mira mira --addr localhost:8080` |
| An agent | `claude mcp add --transport http mira http://localhost:8080/mcp` |
| What is stored | `curl -s http://localhost:8080/api/v1/stats` |
| The objects | `kubectl --context kind-mira-demo -n mira-demo get miracluster,sts,deploy,svc,httproute` |

With the agent connected, ask it *which service is failing checkouts, and why* —
it has [nine tools](agents.md) against the same blocks the UI is reading.

A query enters where everything else does:

```sh
curl -s http://localhost:8080/api/v1/query -H content-type:application/json \
  -d '{"signal":"traces","where":[{"attr":"exception.type","eq":"payments.CardDeclined"}],"limit":3}'
```

## The half nothing exported

`cart` runs `busybox` with a 16Mi limit and no SDK in it. It allocates until the
kernel kills it, backs off, and is killed again — and nothing inside it can say
so. This demo turns the operator's `clusterEvents.endpoint` on, so it watches
Kubernetes Events and container state and posts both to the tier as OTLP logs:

```sh
curl -s http://localhost:8080/api/v1/query -H content-type:application/json \
  -d '{"signal":"logs","where":[{"attr":"otel.scope.name","eq":"mira-operator"}],"limit":5}'
```

```json
{"time_unix_nano":"1789900376000000000","severity_number":17,"severity_text":"ERROR",
 "event_name":"OOMKilled","body":"container cart last terminated: exit code 137",
 "attributes":{"container.image.name":"docker.io/library/busybox:1.37",
   "k8s.container.name":"cart","k8s.container.restart_count":"11",
   "k8s.event.reason":"OOMKilled","k8s.namespace.name":"mira-demo",
   "k8s.object.kind":"Pod","k8s.pod.host_ip":"172.18.0.2",
   "k8s.pod.name":"cart-6d6c669877-zlnwt",
   "k8s.pod.uid":"7f54d237-1c1e-4f3d-99bb-47876fc35901",
   "otel.scope.name":"mira-operator","otel.scope.version":"0.2.0"}}
```

Two sources, one shape: `OOMKilled` and its exit code are the kubelet's account
of the container, `BackOff` and `Unhealthy` come off the Event stream. Both are
keyed on `k8s.pod.uid` — the attribute the k8sattributes processor already puts
on application telemetry — so [an agent](agents.md) writing the RCA reads both
halves with one `query_records`. The Role that flag creates is `get`, `list` and
`watch` on pods and events; there is no value that adds a verb to it.

## Next

- [Install](install.md) — the chart, the image and the flags for a real cluster
- [Connect an agent](agents.md) — what the MCP tools do and how to ask well
- [MiraCluster CRD](reference/crd.md) — every field the operator reads
