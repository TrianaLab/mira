#!/usr/bin/env bash
#
# Mira on a Kind cluster, on http://localhost:8080, in one command.
#
# The difference between this and `e2e/run.sh` is what they are for. That one is
# a gate: it asserts, it tears the cluster down, and it is deliberately awkward
# to poke at. This one asserts nothing and leaves everything running — the
# cluster, the operator, the tier, a generator still producing — because the
# point is a browser tab and an MCP endpoint you can point an agent at.
#
# What it builds, in order:
#
#   1  a one-node Kind cluster with 30080 forwarded to the host's 8080
#   2  Envoy Gateway, and a GatewayClass pinned to that nodePort
#   3  the operator, from charts/mira-operator
#   4  a MiraCluster: one storage replica, one proxy
#   5  a Gateway and an HTTPRoute in front of both
#   6  telemetrygen, exporting into the proxy for as long as the cluster lives
#   7  `cart`, a pod with a memory limit too low to survive, which the operator
#      exports as OOM kills nobody instrumented
#   8  an hour of a four-service shop, seeded through the Gateway from the host
#
# `make demo` is the same product without any of this. Reach for that one first;
# this is for seeing the operator, the proxy and a real ingress on the path.
set -euo pipefail

cd "$(dirname "$0")/../../.."
here=integrations/kubernetes/demo

CLUSTER=${CLUSTER:-mira-demo}
NS=${NS:-mira-demo}
PORT=${PORT:-8080}
ENGINE_IMAGE=mira:demo
OPERATOR_IMAGE=mira-operator:demo
# Pinned here and nowhere else. The docs say "the version run.sh pins" rather
# than a number, so bumping this is a one-line change.
EG_VERSION=${EG_VERSION:-v1.9.1}

URL=http://localhost:$PORT

say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
need() { command -v "$1" >/dev/null 2>&1 || { echo "error: $1 is not installed." >&2; exit 1; }; }
for t in docker kind kubectl helm; do need "$t"; done

# Every kubectl and helm below names this context explicitly. `kind create`
# writes it into the default kubeconfig and switches to it, which is what you
# want afterwards — but a script that applies and deletes objects must not
# follow a context the rest of the machine can move out from under it.
CTX=kind-$CLUSTER
kc() { kubectl --context "$CTX" "$@"; }

# Wait for a command to succeed. Two things here need it and neither is a
# condition `kubectl wait` knows about: Envoy's route table converges a few
# seconds after the HTTPRoute is accepted, and the first ingest has to find a
# proxy that has found a replica.
until_ok() { # <seconds> <message> <command...>
	local deadline=$(( SECONDS + $1 )) secs=$1 msg=$2; shift 2
	while [ "$SECONDS" -lt "$deadline" ]; do
		"$@" >/dev/null 2>&1 && return 0
		sleep 2
	done
	echo "error: $msg (after ${secs}s)" >&2
	return 1
}

if [ "${DOWN:-}" = 1 ]; then
	kind delete cluster --name "$CLUSTER"
	echo "the cluster was the whole of this demo's state, so that is all of it."
	exit 0
fi

# ---------------------------------------------------------------------------
say "cluster"
# ---------------------------------------------------------------------------
if kind get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
	echo "reusing the existing '$CLUSTER'. \`make demo-cluster-down\` deletes it."
else
	# `--config` carries the port forward, so a cluster created any other way
	# will not answer on localhost. That is the one thing not worth reusing.
	kind create cluster --name "$CLUSTER" --config "$here/kind.yaml" --wait 120s
fi

# ---------------------------------------------------------------------------
say "images, and the host binaries"
# ---------------------------------------------------------------------------
# Compiled inside Docker, like the e2e suite's, so this works on a laptop that
# has never built for Linux. `make build` is separate and for the host: it is
# where the seeder and the terminal UI printed at the end come from.
docker build -q -t "$ENGINE_IMAGE" .
docker build -q -t "$OPERATOR_IMAGE" -f integrations/kubernetes/Dockerfile .
kind load docker-image --name "$CLUSTER" "$ENGINE_IMAGE" "$OPERATOR_IMAGE"
make build

# ---------------------------------------------------------------------------
say "Envoy Gateway"
# ---------------------------------------------------------------------------
# The chart ships the Gateway API CRDs, so there is no separate apply for them.
helm --kube-context "$CTX" upgrade --install envoy-gateway \
	oci://docker.io/envoyproxy/gateway-helm --version "$EG_VERSION" \
	--namespace envoy-gateway-system --create-namespace --wait --timeout 300s

# ---------------------------------------------------------------------------
say "operator"
# ---------------------------------------------------------------------------
# The chart as published. `pullPolicy: Never` because the tag only exists inside
# Kind.
#
# `clusterEvents.endpoint` is off in the chart and on here: it is the half of an
# RCA that OTLP never carried — an `OOMKilled`, a `FailedScheduling` — and the
# demo is where it should be visible. The Service it names is three steps below
# and does not exist yet, which costs nothing: a batch that cannot be posted is
# dropped, and the watches keep running.
helm --kube-context "$CTX" upgrade --install mira-operator charts/mira-operator \
	--namespace mira-system --create-namespace \
	--set image.repository=mira-operator \
	--set image.tag=demo \
	--set image.pullPolicy=Never \
	--set "clusterEvents.endpoint=http://tel-proxy.$NS.svc:4318" \
	--wait --timeout 180s

# ---------------------------------------------------------------------------
say "the tier, and the route to it"
# ---------------------------------------------------------------------------
kc create namespace "$NS" --dry-run=client -o yaml | kc apply -f -
# Two applies and not one. `kubectl apply -n X` refuses an object that names
# a different namespace rather than letting it keep its own, so the EnvoyProxy
# in `envoy-gateway-system` cannot travel with the Gateway.
kc apply -f "$here/gatewayclass.yaml"
kc -n "$NS" apply -f "$here/gateway.yaml"
kc -n "$NS" apply -f "$here/miracluster.yaml"
# The operator owns both of these, so neither exists the moment the MiraCluster
# is accepted — and `rollout status` on an object that is not there yet fails
# rather than waiting for it.
until_ok 120 "the operator never created the tier" \
	kc -n "$NS" get statefulset/tel deployment/tel-proxy
kc -n "$NS" rollout status statefulset/tel --timeout=300s
kc -n "$NS" rollout status deployment/tel-proxy --timeout=180s
kc -n "$NS" apply -f "$here/telemetry.yaml"
# The one pod here that is genuinely broken, and the only thing in the demo the
# operator's export is the sole witness to. See `cart.yaml`.
kc -n "$NS" apply -f "$here/cart.yaml"

until_ok 180 "the Gateway never answered on $URL" \
	curl -fs -o /dev/null --max-time 5 "$URL/readyz"
echo "ok: $URL is serving"

# ---------------------------------------------------------------------------
say "an hour of a four-service shop, through the Gateway"
# ---------------------------------------------------------------------------
# From the host and not from a pod, which is the point: it enters at
# localhost:8080 like any other client, so a row coming back out proves the
# whole path — Gateway, HTTPRoute, proxy, replica — and not just the tier.
target/release/examples/loadgen --demo --for 1h --addr "localhost:$PORT"
until_ok 120 "the seeded telemetry never came back out of a query" \
	curl -fs -o /dev/null --max-time 5 "$URL/api/v1/query" \
	-H 'content-type: application/json' -d '{"signal":"traces","limit":1}'

# ---------------------------------------------------------------------------
say "running"
# ---------------------------------------------------------------------------
cat <<TXT

  UI            $URL/
  terminal UI   target/release/mira mira --addr localhost:$PORT
  an agent      claude mcp add --transport http mira $URL/mcp
                then ask it: "which service is failing checkouts, and why?"

  a query       curl -s $URL/api/v1/query -H content-type:application/json \\
                  -d '{"signal":"traces","where":[{"attr":"exception.type","eq":"payments.CardDeclined"}],"limit":3}'
  what is there curl -s $URL/api/v1/stats
  what k8s says curl -s $URL/api/v1/query -H content-type:application/json \\
                  -d '{"signal":"logs","where":[{"attr":"otel.scope.name","eq":"mira-operator"}],"limit":5}'
  the cluster   kubectl --context $CTX -n $NS get miracluster,sts,deploy,svc,httproute

  telemetrygen keeps exporting into the tier, so the numbers move. Nothing
  else has to stay running: this shell is free.

  stop it with  make demo-cluster-down
TXT
