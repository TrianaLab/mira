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
#   D  and down, through the five-step drain: Draining, Job, archive, the claim,
#      and then the Job again — a completed pod pins the claim it mounted
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
# What Envoy Gateway v1.9.1 bundles, which is what `demo/run.sh` installs.
GWAPI_VERSION=${GWAPI_VERSION:-v1.6.1}
HTTP=4318
# Not 4318 locally: `make demo` binds that, and a port-forward that cannot bind
# is a failure fifteen minutes into a run that had nothing wrong with it.
PORT=${PORT:-14318}

# A kubeconfig of this run's own, and the reason is not tidiness. Every kubectl
# below names a namespace and no context, so they follow whatever the *current*
# context is — and that is a global the rest of the machine can move. It moved
# during a run: a phase-D wait timed out on `<empty>` and the diagnostics came
# back "the server doesn't have a resource type miracluster", because by then
# `kubectl` was talking to a production GKE cluster. A suite that deletes
# volumes must not be able to point itself at somebody's cluster, and
# `kubectl config use-context` — which this used to do — is the same bug in
# reverse: it silently repoints the operator's shell at Kind.
#
# `kind` writes here, `kubectl` and `helm` read here, and kube-rs reads
# `KUBECONFIG` too, so `make operator-apiserver` below is pinned by the same
# line.
KUBECONFIG=$(mktemp -t mira-e2e-kubeconfig.XXXXXX)
export KUBECONFIG

pf=
say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
need() { command -v "$1" >/dev/null 2>&1 || { echo "error: $1 is not installed." >&2; exit 1; }; }
for t in docker kind kubectl helm; do need "$t"; done

# Diagnostics before teardown, and only on failure. The two things that explain
# a failed run are what the operator logged and what the objects look like; a
# cluster deleted before either is read is a re-run.
dump() { # <exit-code>
	say "FAILED (exit $1) — state follows"
	kubectl -n "$NS" get miracluster,statefulset,deployment,pod,pvc,job -o wide 2>&1 || true
	kubectl -n "$NS" get events --sort-by=.lastTimestamp 2>&1 | tail -30 || true
	echo "--- operator log"
	kubectl -n mira-system logs deploy/mira-operator --tail=100 2>&1 || true
}
# The exit code has to be read into a local on the *first* line and passed
# around by hand from there. `$?` is whatever the previous command set, so a
# helper that reads it itself reads the status of the `trap -` above it — which
# is how a run that failed in phase A printed no diagnostics and exited 0.
cleanup() {
	local rc=$?
	trap - EXIT
	proxy_down
	[ "$rc" -eq 0 ] || dump "$rc"
	if [ -n "$KEEP" ]; then
		# The run's kubeconfig is a temp file that goes with it, so the
		# first line is how the context reaches the shell you are in.
		echo "KEEP set; cluster '$CLUSTER' left running. Reach it with:"
		echo "  kind export kubeconfig --name $CLUSTER"
		echo "  kubectl --context kind-$CLUSTER -n $NS get miracluster,sts,po,pvc"
		echo "  kind delete cluster --name $CLUSTER"
	else
		kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
	fi
	rm -f "$KUBECONFIG"
	exit "$rc"
}
trap cleanup EXIT

# Wait for a `kubectl get -o jsonpath` expression to equal a value. `kubectl
# wait` cannot do this: it waits on conditions, and "the StatefulSet reports 3
# ready replicas" and "the phase field says Draining" are neither.
until_eq() { # <seconds> <expr> <want> <resource...>
	# `secs` is kept because the message below is printed after the `shift`,
	# and `$1` by then is the first resource — which is how a phase-D timeout
	# reported itself "after statefulset/tels".
	local secs=$1 deadline=$(( SECONDS + $1 )) want=$3 expr=$2 got=; shift 3
	while [ "$SECONDS" -lt "$deadline" ]; do
		got=$(kubectl -n "$NS" get "$@" -o jsonpath="$expr" 2>/dev/null || true)
		[ "$got" = "$want" ] && return 0
		sleep 2
	done
	echo "error: $* $expr was '${got:-<empty>}', wanted '$want' after ${secs}s" >&2
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

# Port-forward the proxy. `cleanup` kills whatever `$pf` names, so a failure in
# the middle of a check does not leave a kubectl behind holding the port against
# the next run.
proxy_up() {
	kubectl -n "$NS" port-forward svc/tel-proxy "$PORT:$HTTP" >/dev/null 2>&1 &
	pf=$!
	until_ok 60 "the proxy never answered on the forwarded port" \
		"curl -fs -o /dev/null http://127.0.0.1:$PORT/health"
}
proxy_down() {
	if [ -n "$pf" ]; then kill "$pf" 2>/dev/null || true; fi
	pf=
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
# Writes the context into this run's own kubeconfig. Needed even right after a
# create, because `--name` on an existing cluster skips the create entirely.
kind export kubeconfig --name "$CLUSTER" >/dev/null

# The Gateway API CRDs, and only the CRDs: `spec.route` writes an `HTTPRoute`,
# which the API server rejects with a 404 on the resource path if the kind is
# not registered. No controller is installed with them — nothing here asks for
# the route to be *programmed*, only for the API server to accept and store it.
# Same bundle version Envoy Gateway ships in `demo/run.sh`, so the two clusters
# validate against the same schema.
kubectl apply --server-side -f \
	"https://github.com/kubernetes-sigs/gateway-api/releases/download/${GWAPI_VERSION}/standard-install.yaml" \
	>/dev/null
kubectl wait --for=condition=established --timeout=60s \
	crd/httproutes.gateway.networking.k8s.io >/dev/null

# ---------------------------------------------------------------------------
say "reconcile against the API server"
# ---------------------------------------------------------------------------
# Here and not at the end, because these need the cluster and nothing else. A
# CRD the API server prunes a field out of, or a StatefulSet it returns 422 for,
# fails thirty seconds in rather than after two container builds — and those are
# the failures most likely to be waiting, since they are the ones a fake client
# cannot see.
make operator-apiserver

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

# The lease, with the real RBAC and the real downward API behind it. The
# operator reconciles either way if this rule or the POD_NAME env var is wrong
# — it just stops being the only one that does, which nothing else here would
# catch.
kubectl -n mira-system wait --for=jsonpath='{.spec.holderIdentity}' \
	lease/mira-operator --timeout=60s
kubectl -n mira-system get lease/mira-operator \
	-o jsonpath='{.spec.holderIdentity}' | grep -q '^mira-operator-'
echo "ok: one operator holds the lease, under its own pod name"

kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f -

# ---------------------------------------------------------------------------
say "A — a MiraCluster becomes a tier"
# ---------------------------------------------------------------------------
sed "s|image: mira:e2e|image: $ENGINE_IMAGE|" "$here/miracluster.yaml" | kubectl -n "$NS" apply -f -

until_eq 300 '{.status.readyReplicas}' 2 statefulset/tel
until_eq 180 '{.status.readyReplicas}' 1 deployment/tel-proxy
# Applied by the operator and by nothing else: the CR named none of these.
kubectl -n "$NS" get configmap/tel configmap/tel-proxy service/tel-headless service/tel-proxy \
	poddisruptionbudget/tel >/dev/null
echo "ok: StatefulSet 2/2, proxy 1/1, five generated objects present"

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
#
# Traces and logs only, because those are what a proxy can answer. Metrics are
# below, and asked of the replicas.
proxy_up
MIRA_URL="http://127.0.0.1:$PORT" scripts/wait-for-signals.sh 240 assert traces logs
proxy_down

# The third signal, asked of the replicas rather than of the proxy, because
# `/api/v1/metrics/names` is a documented 501 there: it is built by walking one
# node's blocks and there is no cursor to merge two nodes' answers on. Papering
# over that with a proxy round trip would test a merge the product does not
# claim; asking both replicas tests what it does claim, which is that the
# export landed somewhere in the tier.
#
# In-cluster rather than through another port-forward, because the routing hash
# is free to put `billing` on either replica and nothing here may depend on
# which — a forward to one pod would be a coin flip.
metrics_check() { # <replica...>
	kubectl -n "$NS" delete pod metrics-check --ignore-not-found >/dev/null
	sed "s|REPLICAS|$*|" <<-'YAML' | kubectl -n "$NS" apply -f -
		apiVersion: v1
		kind: Pod
		metadata:
		  name: metrics-check
		spec:
		  restartPolicy: Never
		  containers:
		    - name: check
		      image: busybox:1.37
		      command: ["sh", "-c"]
		      args:
		        - |
		          i=0
		          while [ $i -lt 90 ]; do
		            for r in REPLICAS; do
		              url="http://$r.tel-headless:4318/api/v1/metrics/names"
		              out=$(wget -q -O - --post-data='{}' \
		                --header='content-type: application/yaml' "$url") || continue
		              case "$out" in
		                *'"names":[]'*) ;;
		                *) echo "$r answered: $out"; exit 0 ;;
		              esac
		            done
		            i=$((i + 1)); sleep 2
		          done
		          echo "no replica has a metric name; the metrics export never landed"
		          exit 1
	YAML
	kubectl -n "$NS" wait --for=jsonpath='{.status.phase}'=Succeeded pod/metrics-check --timeout=240s \
		|| { kubectl -n "$NS" logs metrics-check || true; echo "error: metrics never reached a replica" >&2; exit 1; }
	kubectl -n "$NS" logs metrics-check
}
metrics_check tel-0 tel-1

# ---------------------------------------------------------------------------
say "C — the tier follows the floor up"
# ---------------------------------------------------------------------------
# Raising `spec.replicas` grows the tier with no threshold involved: the floor
# is applied before the scale decision is even read, because a floor that is not
# met is not a floor. Coming back down is not the mirror of this and phase D
# says why.
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
# Lowering the floor does not shrink the tier; it lets the tier shrink. The
# drain archives a volume and then deletes it, so the operator waits for the
# replicas to agree the data fits without that volume before it starts one, and
# the manifest's `downWhenFreeAbove: 0.002` is what makes that agreement
# unconditional here rather than a reading off a Kind node's disk. The first
# version of this phase patched the floor and waited five minutes for a scale-in
# the operator had no reason to perform.
kubectl -n "$NS" patch miracluster tel --type merge -p '{"spec":{"replicas":2}}'
until_eq 300 '{.status.readyReplicas}' 2 statefulset/tel
# The claim outlives the pod, gets archived, and only then goes. Waiting on the
# phase rather than on the Job because the Job is deleted with the cluster and
# the phase is the operator's own account of what it did.
until_eq 300 '{.status.phase}' Ready miracluster/tel
# Waited for rather than asserted once. The operator issues the delete and then
# writes `Ready`, but a claim with a pod that mounted it still on the node sits
# in `Terminating` behind its `pvc-protection` finalizer for a few seconds after
# that — long enough that a single `get` right on the phase edge fails against a
# drain that worked perfectly.
# shellcheck disable=SC2016  # re-evaluated each round by until_ok
until_ok 120 "the drained replica's claim is still there" \
	'! kubectl -n "$NS" get pvc data-tel-2'

# The operator's own account of the tier, which is not the StatefulSet's. Both
# status patches in a drain are server-side applies by one field manager, so a
# key the second one leaves out is a key the first one loses: this read 0 beside
# two running pods until `finish_drain` repeated the count.
until_eq 60 '{.status.replicas}' 2 miracluster/tel

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
MIRA_URL="http://127.0.0.1:$PORT" scripts/wait-for-signals.sh 120 assert traces logs
proxy_down
metrics_check tel-0 tel-1
echo "ok: the tier still ingests and serves with no controller in the cluster"

say "all five passed"
