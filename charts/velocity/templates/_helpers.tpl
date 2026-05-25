{{/* ───────────────────────────── names ───────────────────────────── */}}
{{- define "velocity.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "velocity.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "velocity.operator.fullname" -}}
{{- printf "%s-operator" (include "velocity.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "velocity.webhook.fullname" -}}
{{- printf "%s-webhook" (include "velocity.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "velocity.logProcessor.fullname" -}}
{{- printf "%s-log-processor" (include "velocity.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "velocity.logCollector.fullname" -}}
{{- printf "%s-log-collector" (include "velocity.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "velocity.api.fullname" -}}
{{- printf "%s-api" (include "velocity.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "velocity.api.selectorLabels" -}}
app.kubernetes.io/name: {{ include "velocity.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: api
{{- end -}}

{{- define "velocity.api.image" -}}
{{- $r := .Values.image.registry | trimSuffix "/" -}}
{{- $repo := .Values.image.repository | trimSuffix "/" -}}
{{- printf "%s/%s/%s:%s" $r $repo .Values.api.image.name .Values.api.image.tag -}}
{{- end -}}

{{/* ADR-011: operator-materialised data-API image. Config lives under
     operator.dataApi so the operator controls the image it spawns. */}}
{{- define "velocity.dataApi.image" -}}
{{- $r := .Values.image.registry | trimSuffix "/" -}}
{{- $repo := .Values.image.repository | trimSuffix "/" -}}
{{- printf "%s/%s/%s:%s" $r $repo .Values.operator.dataApi.image.name .Values.operator.dataApi.image.tag -}}
{{- end -}}

{{- define "velocity.search.fullname" -}}
{{- printf "%s-search" (include "velocity.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "velocity.search.selectorLabels" -}}
app.kubernetes.io/name: {{ include "velocity.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: search
{{- end -}}

{{- define "velocity.search.image" -}}
{{- $r := .Values.image.registry | trimSuffix "/" -}}
{{- $repo := .Values.image.repository | trimSuffix "/" -}}
{{- printf "%s/%s/%s:%s" $r $repo .Values.api.search.image.name .Values.api.search.image.tag -}}
{{- end -}}

{{- define "velocity.warmReader.fullname" -}}
{{- printf "%s-warm-reader" (include "velocity.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "velocity.warmReader.selectorLabels" -}}
app.kubernetes.io/name: {{ include "velocity.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: warm-reader
{{- end -}}

{{- define "velocity.warmReader.image" -}}
{{- $r := .Values.image.registry | trimSuffix "/" -}}
{{- $repo := .Values.image.repository | trimSuffix "/" -}}
{{- printf "%s/%s/%s:%s" $r $repo .Values.warmReader.image.name .Values.warmReader.image.tag -}}
{{- end -}}

{{- define "velocity.archiveWorker.fullname" -}}
{{- printf "%s-archive-worker" (include "velocity.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "velocity.archiveWorker.selectorLabels" -}}
app.kubernetes.io/name: {{ include "velocity.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: archive-worker
{{- end -}}

{{- define "velocity.archiveWorker.image" -}}
{{- $r := .Values.image.registry | trimSuffix "/" -}}
{{- $repo := .Values.image.repository | trimSuffix "/" -}}
{{- printf "%s/%s/%s:%s" $r $repo .Values.archiveWorker.image.name .Values.archiveWorker.image.tag -}}
{{- end -}}

{{- define "velocity.logProcessor.selectorLabels" -}}
app.kubernetes.io/name: {{ include "velocity.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: log-processor
{{- end -}}

{{- define "velocity.logCollector.selectorLabels" -}}
app.kubernetes.io/name: {{ include "velocity.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: log-collector
{{- end -}}

{{- define "velocity.logProcessor.image" -}}
{{- $r := .Values.image.registry | trimSuffix "/" -}}
{{- $repo := .Values.image.repository | trimSuffix "/" -}}
{{- printf "%s/%s/%s:%s" $r $repo .Values.logProcessor.image.name .Values.logProcessor.image.tag -}}
{{- end -}}

{{- define "velocity.logCollector.image" -}}
{{- $r := .Values.image.registry | trimSuffix "/" -}}
{{- $repo := .Values.image.repository | trimSuffix "/" -}}
{{- printf "%s/%s/%s:%s" $r $repo .Values.logCollector.image.name .Values.logCollector.image.tag -}}
{{- end -}}

{{- define "velocity.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/* ─────────────────────── labels / selectors ────────────────────── */}}
{{- define "velocity.labels" -}}
helm.sh/chart: {{ include "velocity.chart" . }}
app.kubernetes.io/name: {{ include "velocity.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: velocity
{{- end -}}

{{- define "velocity.operator.selectorLabels" -}}
app.kubernetes.io/name: {{ include "velocity.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: operator
{{- end -}}

{{- define "velocity.webhook.selectorLabels" -}}
app.kubernetes.io/name: {{ include "velocity.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: webhook
{{- end -}}

{{/* ─────────────────────── service account ───────────────────────── */}}
{{- define "velocity.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "velocity.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{/* ─────────────────────── image refs ────────────────────────────── */}}
{{- define "velocity.operator.image" -}}
{{- $r := .Values.image.registry | trimSuffix "/" -}}
{{- $repo := .Values.image.repository | trimSuffix "/" -}}
{{- printf "%s/%s/%s:%s" $r $repo .Values.operator.image.name .Values.operator.image.tag -}}
{{- end -}}

{{- define "velocity.webhook.image" -}}
{{- $r := .Values.image.registry | trimSuffix "/" -}}
{{- $repo := .Values.image.repository | trimSuffix "/" -}}
{{- printf "%s/%s/%s:%s" $r $repo .Values.webhook.image.name .Values.webhook.image.tag -}}
{{- end -}}

{{/* ─────────────────────── database URL ──────────────────────────── */}}
{{- define "velocity.databaseUrl" -}}
{{- with .Values.operator.database -}}
{{- if .url -}}
{{ .url }}
{{- else -}}
postgres://{{ .user }}@{{ .host }}:{{ .port }}/{{ .db }}
{{- end -}}
{{- end -}}
{{- end -}}
