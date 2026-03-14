{{/*
Common labels
*/}}
{{- define "synth.labels" -}}
app.kubernetes.io/name: synth
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/*
Selector labels
*/}}
{{- define "synth.selectorLabels" -}}
app: synth
app.kubernetes.io/name: synth
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Service account name
*/}}
{{- define "synth.serviceAccountName" -}}
{{- if .Values.serviceAccount.name -}}
{{- .Values.serviceAccount.name -}}
{{- else -}}
synth
{{- end -}}
{{- end }}
