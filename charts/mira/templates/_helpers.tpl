{{/*
Expand the name of the chart.
*/}}
{{- define "mira.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name.
*/}}
{{- define "mira.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Chart name and version, for the chart label.
*/}}
{{- define "mira.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Common labels
*/}}
{{- define "mira.labels" -}}
helm.sh/chart: {{ include "mira.chart" . }}
{{ include "mira.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/*
Selector labels
*/}}
{{- define "mira.selectorLabels" -}}
app.kubernetes.io/name: {{ include "mira.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
The name of the ServiceAccount to use.
*/}}
{{- define "mira.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "mira.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/*
The headless Service. A StatefulSet needs one to hand out per-pod DNS, and that
DNS is the only way to address a specific replica — which matters here because a
query is answered from one replica's own blocks, with no fan-out.
*/}}
{{- define "mira.headlessServiceName" -}}
{{- printf "%s-headless" (include "mira.fullname" .) | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Mira's config file, in KYAML.

KYAML is a strict subset of YAML 1.2: explicit `{}`, every string quoted,
indentation for the reader only (docs/config.md). Mira reads every value as a
string and refuses anything that arrived as another type, so `wal` — a bool in
values.yaml, because that is what it is to whoever sets it — is rendered as the
quoted string the parser demands. `quote` on every scalar is the whole trick.

One definition, used by the ConfigMap and by the checksum annotation that rolls
the pods when it changes — which is also why the alerting guard lives here: it
is the one thing both templates go through.
*/}}
{{- define "mira.config" -}}
{{- if and .Values.config.alerts.rules (gt (int .Values.replicaCount) 1) }}
{{- fail "config.alerts.rules is set with replicaCount > 1. Mira holds no coordination state, so nothing elects an evaluator: every replica would evaluate the same rules against its own blocks and page separately. Run the evaluating replica as its own release with replicaCount: 1 (docs/config.md, \"alerts.rules\")." }}
{{- end }}
{
  "node": {{ .Values.config.node | quote }},
  "listen": {
    "grpc": "0.0.0.0:4317",
    "http": "0.0.0.0:4318",
  },
  "storage": {
    "dir": "/data",
    "retention": {{ .Values.config.storage.retention | quote }},
  },
  "ingest": {
    "max_request_bytes": {{ .Values.config.ingest.maxRequestBytes | quote }},
    "queue": {{ .Values.config.ingest.queue | toString | quote }},
    "shards": {{ .Values.config.ingest.shards | toString | quote }},
    "wal": {{ .Values.config.ingest.wal | toString | quote }},
  },
  "telemetry": {
    "self": {{ .Values.config.telemetry.self | toString | quote }},
    "interval": {{ .Values.config.telemetry.interval | quote }},
  },
{{- if .Values.config.alerts.rules }}
  "alerts": {
    "rules": "/etc/mira/alerts.kyaml",
  },
{{- end }}
}
{{- end }}
