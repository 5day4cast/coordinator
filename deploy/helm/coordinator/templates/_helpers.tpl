{{- define "coordinator.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "coordinator.fullname" -}}
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

{{- define "coordinator.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "coordinator.labels" -}}
helm.sh/chart: {{ include "coordinator.chart" . }}
{{ include "coordinator.selectorLabels" . }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "coordinator.selectorLabels" -}}
app.kubernetes.io/name: {{ include "coordinator.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "coordinator.serviceAccountName" -}}
{{- if .Values.serviceAccount.name }}
{{- .Values.serviceAccount.name }}
{{- else }}
{{- include "coordinator.fullname" . }}
{{- end }}
{{- end }}

{{/*
Deployment name - includes slot suffix when blue/green is enabled
*/}}
{{- define "coordinator.deploymentName" -}}
{{- if .Values.blueGreen.enabled }}
{{- printf "%s-%s" (include "coordinator.fullname" .) .Values.blueGreen.slot }}
{{- else }}
{{- include "coordinator.fullname" . }}
{{- end }}
{{- end }}

{{/*
PVC name - includes slot suffix when blue/green is enabled
*/}}
{{- define "coordinator.pvcName" -}}
{{- if .Values.blueGreen.enabled }}
{{- printf "%s-%s" (include "coordinator.fullname" .) .Values.blueGreen.slot }}
{{- else }}
{{- include "coordinator.fullname" . }}
{{- end }}
{{- end }}

{{/*
Deployment selector labels - includes slot when blue/green is enabled
*/}}
{{- define "coordinator.deploymentSelectorLabels" -}}
{{ include "coordinator.selectorLabels" . }}
{{- if .Values.blueGreen.enabled }}
app.kubernetes.io/slot: {{ .Values.blueGreen.slot }}
{{- end }}
{{- end }}

{{/*
Service selector labels - uses activeSlot when blue/green is enabled
*/}}
{{- define "coordinator.serviceSelectorLabels" -}}
{{ include "coordinator.selectorLabels" . }}
{{- if .Values.blueGreen.enabled }}
app.kubernetes.io/slot: {{ .Values.blueGreen.activeSlot }}
{{- end }}
{{- end }}
