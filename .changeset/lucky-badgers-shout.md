---
"@mira/operator": patch
---

The headless Service addresses the storage tier and not the proxy. It selected
on the label subset every pod under a `MiraCluster` carries, so the proxy was in
its endpoints too — and because both answer `/api/v1/query`, a client that
resolved the Service name rather than a pod name got merged results on some
connections and node-local results on others. The per-pod names the operator and
the proxy use (`tel-0.tel-headless`) come from the StatefulSet's `serviceName`
and were never affected.
