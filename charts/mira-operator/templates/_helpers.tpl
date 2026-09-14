{{/* Chart name, overridable. */}}
{{- define "mira-operator.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Fully qualified name, capped at 63 characters because it becomes a label value.
*/}}
{{- define "mira-operator.fullname" -}}
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

{{- define "mira-operator.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "mira-operator.labels" -}}
helm.sh/chart: {{ include "mira-operator.chart" . }}
{{ include "mira-operator.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/component: operator
{{- end }}

{{/*
The selector, deliberately without the version label.

A Deployment's `spec.selector` is immutable, so a version in it makes the first
`helm upgrade` fail with a message about an immutable field and leave the
release wedged.
*/}}
{{- define "mira-operator.selectorLabels" -}}
app.kubernetes.io/name: {{ include "mira-operator.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "mira-operator.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "mira-operator.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}
