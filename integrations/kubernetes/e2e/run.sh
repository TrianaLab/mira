#!/usr/bin/env bash
#
# The operator end to end, on a real cluster.
#
# This replaced the compose-based `make e2e` rather than joining it. That one
# put a stock collector in front of one Mira container and asserted three
# signals came back, which is a real test of the OTLP surface and no test at all
# of the thing that now decides how many Miras there are. Everything it asserted
# is asserted here, through a tier the operator built, so there is one e2e.
#
# What it checks, in order:
#
#   A  a MiraCluster becomes a running tier — StatefulSet, proxy, Services,
#      ConfigMaps — with nothing but the CR applied
#   B  a stock OpenTelemetry Collector can export into that tier and all three
#      signals come back out of the proxy
#   C  the tier follows `spec.replicas` up
#   D  and down, through the four-step drain: Draining, Job, archive, and only
#      then the claim
#   E  deleting the operator does not stop the tier serving
#
# E is the assertion the architecture rests on. Principle 4 says Mira holds no
# coordination state, and the defence of shipping a controller at all is that a
# controller is not Mira. That is a testable claim, so it is tested.
set -euo pipefail

cd "$(dirname "$0")/../../.."
here=integrations/kubernetes/e2e

CLUSTER=${CLUSTER:-mira-operator-e2e}
NS=${NS:-mira-e2e}
KEEP=${KEEP:-}
ENGINE_IMAGE=mira:e2e
OPERATOR_IMAGE=mira-operator:e2e
HTTP=4318
# Not 4318 locally: `make demo` binds that, and a port-forward that cannot bind
# is a failure fifteen minutes into a run that had nothing wrong with it.
PORT=${PORT:-14318}

say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
need() { command -v "$1" >/dev/null 2>&1 || { echo "error: $1 is not installed." >&2; exit 1; }; }
for t in docker kind kubectl helm; do need "$t"; done

# Diagnostics before teardown, and only on failure. The two things that explain
# a failed run are what the operator logged and what the objects look like; a
# cluster deleted before either is read is a re-run.
dump() {
	rc=$?
	[ "$rc" -eq 0 ] && return 0
	say "FAILED (exit $rc) — state follows"
	kubectl -n "$NS" get miracluster,statefulset,deployment,pod,pvc,job -o wide 2>&1 || true
	kubectl -n "$NS" get events --sort-by=.lastTimestamp 2>&1 | tail -30 || true
	echo "--- operator log"
	kubectl -n mira-system logs deploy/mira-operator --tail=100 2>&1 || true
	return "$rc"
}
cleanup() {
	rc=$?
	trap - EXIT
	dump || rc=$?
	if [ -n "$KEEP" ]; then
		echo "KEEP set; cluster '$CLUSTER' left running. Delete it with: kind delete cluster --name $CLUSTER"
	else
		kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
	fi
	exit "$rc"
}
trap cleanup EXIT

# Wait for a `kubectl get -o jsonpath` expression to equal a value. `kubectl
# wait` cannot do this: it waits on conditions, and "the StatefulSet reports 3
# ready replicas" and "the phase field says Draining" are neither.
until_eq() { # <seconds> <expr> <want> <resource...>
	local deadline=$(( SECONDS + $1 )) want=$3 expr=$2 got=; shift 3
	while [ "$SECONDS" -lt "$deadline" ]; do
		got=$(kubectl -n "$NS" get "$@" -o jsonpath="$expr" 2>/dev/null || true)
		[ "$got" = "$want" ] && return 0
		sleep 2
	done
	echo "error: $* $expr was '${got:-<empty>}', wanted '$want' after ${1}s" >&2
	return 1
}

# The same loop for a whole command rather than a field. The command arrives as
# a string and is re-evaluated each round, so callers quote it with single
# quotes on purpose — SC2016 is the point, not a slip.
until_ok() { # <seconds> <message> <command>
	local deadline=$(( SECONDS + $1 )) msg=$2 cmd=$3
	while [ "$SECONDS" -lt "$deadline" ]; do
		eval "$cmd" >/dev/null 2>&1 && return 0
		sleep 2
	done
	echo "error: $msg (after ${1}s)" >&2
	return 1
}

# Port-forward the proxy. The forward joins the EXIT trap while it is up, so a
# failure in the middle of a check does not leave a kubectl behind holding the
# port against the next run.
pf=
proxy_up() {
	kubectl -n "$NS" port-forward svc/tel-proxy "$PORT:$HTTP" >/dev/null 2>&1 &
	pf=$!
	trap 'proxy_down; cleanup' EXIT
	until_ok 60 "the proxy never answered on the forwarded port" \
		"curl -fs -o /dev/null http://127.0.0.1:$PORT/health"
}
proxy_down() {
	[ -n "$pf" ] && kill "$pf" 2>/dev/null
	pf=
	trap cleanup EXIT
	return 0
}

# One query document, posted the way the CLI and the MCP tool post it, asserted
# on by looking for a needle in the response rather than for the absence of an
# empty `rows` — a 500 and an empty result must not read the same here.
query_has() { # <body> <needle>
	curl -fs -H 'content-type: application/yaml' --data "$1" \
		"http://127.0.0.1:$PORT/api/v1/query" | grep -q "$2"
}

# ---------------------------------------------------------------------------
say "cluster"
# ---------------------------------------------------------------------------
kind get clusters 2>/dev/null | grep -qx "$CLUSTER" \
	|| kind create cluster --name "$CLUSTER" --config "$here/kind.yaml" --wait 120s
kubectl config use-context "kind-$CLUSTER" >/dev/null

# ---------------------------------------------------------------------------
say "images"
# ---------------------------------------------------------------------------
# Compiled inside Docker rather than copied in from the host. `make e2e` used
# --build-arg BIN=prebuilt because CI had already built a Linux binary; here the
# point is that this runs on a laptop too, and a Mach-O binary in a Linux image
# fails 240 seconds later as silence rather than as an error.
docker build -q -t "$ENGINE_IMAGE" .
docker build -q -t "$OPERATOR_IMAGE" -f integrations/kubernetes/Dockerfile .
kind load docker-image --name "$CLUSTER" "$ENGINE_IMAGE" "$OPERATOR_IMAGE"

# ---------------------------------------------------------------------------
say "operator"
# ---------------------------------------------------------------------------
# The chart as published, not a hand-rolled Deployment — so a broken template,
# a missing RBAC rule or a CRD the chart forgot to ship fails here rather than
# in somebody's cluster. `pullPolicy: Never` because the tag only exists inside
# Kind; IfNotPresent would also work and would silently reach for the registry
# the day the local image is missing.
helm upgrade --install mira-operator charts/mira-operator \
	--namespace mira-system --create-namespace \
	--set image.repository=mira-operator \
	--set image.tag=e2e \
	--set image.pullPolicy=Never \
	--wait --timeout 120s

kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f -

# ---------------------------------------------------------------------------
say "A — a MiraCluster becomes a tier"
# ---------------------------------------------------------------------------
sed "s|image: mira:e2e|image: $ENGINE_IMAGE|" "$here/miracluster.yaml" | kubectl -n "$NS" apply -f -

until_eq 300 '{.status.readyReplicas}' 2 statefulset/tel
until_eq 180 '{.status.readyReplicas}' 1 deployment/tel-proxy
# Applied by the operator and by nothing else: the CR named none of these.
kubectl -n "$NS" get configmap/tel configmap/tel-proxy service/tel-headless service/tel-proxy >/dev/null
echo "ok: StatefulSet 2/2, proxy 1/1, four generated objects present"

# ---------------------------------------------------------------------------
say "B — a stock collector exports into it, and all three signals come back"
# ---------------------------------------------------------------------------
kubectl -n "$NS" apply -f "$here/telemetry.yaml"
kubectl -n "$NS" rollout status deploy/otelcol --timeout=180s
kubectl -n "$NS" apply -f "$here/generators.yaml"
for j in gen-traces gen-errors gen-logs gen-metrics; do
	kubectl -n "$NS" wait --for=condition=complete "job/$j" --timeout=300s
done

# Through the proxy, which is the only address the collector was given. A row
# here means the whole path worked: generator, stock collector, proxy, `route`,
# a storage replica, a sealed block, and the query coming back merged.
proxy_up
MIRA_URL="http://127.0.0.1:$PORT" scripts/wait-for-signals.sh 240 assert
proxy_down

# ---------------------------------------------------------------------------
say "C — the tier follows spec.replicas up"
# ---------------------------------------------------------------------------
kubectl -n "$NS" patch miracluster tel --type merge -p '{"spec":{"replicas":3}}'
until_eq 300 '{.status.readyReplicas}' 3 statefulset/tel
# The proxy's config is regenerated from the live count and its pod template is
# annotated with a hash of it, so a scale that did not reach the proxy leaves
# three replicas ingesting and a proxy that still queries two.
# shellcheck disable=SC2016  # re-evaluated each round by until_ok
until_ok 120 "the proxy config never learned about replica 2" \
	'kubectl -n "$NS" get cm tel-proxy -o jsonpath="{.data.mira\.yaml}" | grep -q tel-2.tel-headless'
echo "ok: 3/3 and the proxy knows about all three"

# Straight at replica 2, past the proxy, past the routing hash. Two things need
# this and neither can be had from the corpus above, because `hash(resource)` is
# free to put every service on one replica and nothing here may depend on which:
#
#   * the merge. A row that provably lives on exactly one replica, asked for
#     through the proxy, is the only honest test that the proxy is merging
#     rather than forwarding.
#   * the drain, below. A replica holding nothing archives nothing, and an
#     empty cold volume would then read as a pass.
kubectl -n "$NS" apply -f - <<-'YAML'
	apiVersion: batch/v1
	kind: Job
	metadata: { name: gen-only-on-2 }
	spec:
	  backoffLimit: 12
	  template:
	    spec:
	      restartPolicy: OnFailure
	      containers:
	        - name: gen
	          image: ghcr.io/open-telemetry/opentelemetry-collector-contrib/telemetrygen:v0.142.0
	          args: ["traces", "--otlp-endpoint", "tel-2.tel-headless:4317",
	                 "--otlp-insecure", "--rate", "0", "--traces", "50",
	                 "--service", "only-on-2"]
YAML
kubectl -n "$NS" wait --for=condition=complete job/gen-only-on-2 --timeout=300s

# shellcheck disable=SC2034  # expanded inside until_ok's eval, not here
only_on_2='{"signal":"traces","from":"-15m","to":"now","where":[{"attr":"service.name","eq":"only-on-2"}],"limit":1}'
proxy_up
# shellcheck disable=SC2016  # $only_on_2 must expand at eval time, not now
until_ok 120 "the proxy never returned the rows only replica 2 holds — it is forwarding, not merging" \
	'query_has "$only_on_2" only-on-2'
proxy_down
echo "ok: a row held by one replica came back through the proxy"

# ---------------------------------------------------------------------------
say "D — and down, through the drain"
# ---------------------------------------------------------------------------
kubectl -n "$NS" patch miracluster tel --type merge -p '{"spec":{"replicas":2}}'
until_eq 300 '{.status.readyReplicas}' 2 statefulset/tel
# The claim outlives the pod, gets archived, and only then goes. Waiting on the
# phase rather than on the Job because the Job is deleted with the cluster and
# the phase is the operator's own account of what it did.
until_eq 300 '{.status.phase}' Ready miracluster/tel
kubectl -n "$NS" get pvc data-tel-2 >/dev/null 2>&1 \
	&& { echo "error: the drained replica's claim is still there" >&2; exit 1; }

# The archive itself. Everything above this line passes just as happily when
# the drain Job wrote nothing at all: the claim is deleted either way, the
# phase says Ready either way, and the only place the difference is visible is
# on the cold volume. `/cold/tel-2` and not `/cold`, because the path the
# replica was writing to and the path the Job writes to have to be one path —
# they were two until `${node}` was expanded on the way into the Job's args.
kubectl -n "$NS" delete pod cold-check --ignore-not-found >/dev/null
kubectl -n "$NS" apply -f - <<-'YAML'
	apiVersion: v1
	kind: Pod
	metadata:
	  name: cold-check
	spec:
	  restartPolicy: Never
	  containers:
	    - name: check
	      image: busybox:1.37
	      command: ["sh", "-c"]
	      args:
	        - |
	          ls -R /cold
	          test -d /cold/tel-2 || { echo "no /cold/tel-2 — the drain archived nowhere the replica was writing"; exit 1; }
	          find /cold/tel-2 -name '*.arrow' | head -5 | grep -q . \
	            || { echo "/cold/tel-2 holds no block — the claim was deleted unarchived"; exit 1; }
	      volumeMounts:
	        - {name: cold, mountPath: /cold}
	  volumes:
	    - name: cold
	      persistentVolumeClaim: {claimName: mira-cold}
YAML
kubectl -n "$NS" wait --for=jsonpath='{.status.phase}'=Succeeded pod/cold-check --timeout=120s \
	|| { kubectl -n "$NS" logs cold-check || true; echo "error: the archive is not on the cold volume" >&2; exit 1; }
kubectl -n "$NS" logs cold-check
echo "ok: claim gone, archive present under /cold/tel-2"

# ---------------------------------------------------------------------------
say "E — deleting the operator does not stop the tier"
# ---------------------------------------------------------------------------
# The delete-test for principle 4. If this fails, a Mira pod is asking the
# operator something, and the argument for shipping a controller at all is
# wrong.
helm uninstall mira-operator --namespace mira-system --wait
proxy_up
MIRA_URL="http://127.0.0.1:$PORT" scripts/wait-for-signals.sh 120 assert
proxy_down
echo "ok: the tier still ingests and serves with no controller in the cluster"

say "all five passed"
