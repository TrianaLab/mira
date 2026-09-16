---
"@mira/operator": minor
---

`spec.route` on a `MiraCluster` writes an `HTTPRoute` in front of the tier. It
names the Gateways to attach to and, optionally, the hostnames; what the route
*says* is derived — OTLP ingest and the merged read go to the proxy, everything
else to a storage node, which is the only place the UI and `/mcp` are answered.
Removing the field removes the route. Unset creates nothing, so a cluster
without the Gateway API CRDs is unaffected.
