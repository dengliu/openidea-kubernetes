#!/usr/bin/env bash
# Idempotently seeds a local Kubernetes cluster (default: docker-desktop) with
# realistic workloads & Helm releases for the OpenIDEA Kubernetes showcase video.
set -euo pipefail

export PATH="/opt/homebrew/bin:/usr/local/bin:$PATH"
CTX="${KUBE_CONTEXT:-docker-desktop}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
KUBE_APP_ROOT="${KUBE_APP_ROOT:-$SCRIPT_DIR/fixtures/kube-app}"

echo "==> Checking cluster connectivity ($CTX)..."
kubectl --context "$CTX" cluster-info >/dev/null

echo "==> Ensuring Helm repositories..."
helm repo add podinfo https://stefanprodan.github.io/podinfo --force-update >/dev/null
helm repo add prometheus-community https://prometheus-community.github.io/helm-charts --force-update >/dev/null
helm repo add ingress-nginx https://kubernetes.github.io/ingress-nginx --force-update >/dev/null

echo "==> Installing / refreshing podinfo (default)..."
helm --kube-context "$CTX" upgrade --install podinfo podinfo/podinfo \
  -n default --set replicaCount=1 >/dev/null

echo "==> Installing / refreshing node-exporter (monitoring)..."
helm --kube-context "$CTX" upgrade --install node-exporter prometheus-community/prometheus-node-exporter \
  -n monitoring --create-namespace --set prometheus.monitor.enabled=false >/dev/null

echo "==> Installing / refreshing redis (cache)..."
helm --kube-context "$CTX" upgrade --install redis oci://registry-1.docker.io/bitnamicharts/redis \
  -n cache --create-namespace \
  --set architecture=standalone \
  --set auth.enabled=false \
  --set master.persistence.enabled=false >/dev/null

echo "==> Installing / refreshing ingress-nginx (ingress-nginx)..."
helm --kube-context "$CTX" upgrade --install ingress-nginx ingress-nginx/ingress-nginx \
  -n ingress-nginx --create-namespace \
  --set controller.service.type=ClusterIP \
  --set controller.admissionWebhooks.enabled=false >/dev/null

echo "==> Installing / refreshing scylladb (scylla)..."
TMP_SCYLLA="$(mktemp -d)"
trap 'rm -rf "$TMP_SCYLLA"' EXIT
mkdir -p "$TMP_SCYLLA/scylladb/templates"

cat <<'YAML' > "$TMP_SCYLLA/scylladb/Chart.yaml"
apiVersion: v2
name: scylladb
description: High-performance NoSQL wide-column database (CQL & Alternator compatible)
type: application
version: 6.2.0
appVersion: "6.2.0"
YAML

cat <<'YAML' > "$TMP_SCYLLA/scylladb/values.yaml"
replicas: 1
image:
  repository: scylladb/scylla
  tag: "6.2"
  pullPolicy: IfNotPresent
datacenter: us-west-2
rack: rack1
developerMode: true
smp: 1
memory: 512M
cqlPort: 9042
metricsPort: 9180
YAML

cat <<'TXT' > "$TMP_SCYLLA/scylladb/templates/NOTES.txt"
ScyllaDB {{ .Chart.AppVersion }} has been deployed to namespace "{{ .Release.Namespace }}" (DC: {{ .Values.datacenter }}, Rack: {{ .Values.rack }}).

1. Connect with CQL shell inside the cluster:
   kubectl exec -it -n {{ .Release.Namespace }} statefulset/scylladb -- cqlsh

2. Scrape Prometheus metrics on port {{ .Values.metricsPort }}:
   http://scylladb.{{ .Release.Namespace }}.svc.cluster.local:{{ .Values.metricsPort }}/metrics
TXT

cat <<'YAML' > "$TMP_SCYLLA/scylladb/templates/service.yaml"
apiVersion: v1
kind: Service
metadata:
  name: scylladb
  namespace: {{ .Release.Namespace }}
  labels:
    app.kubernetes.io/name: scylladb
    app.kubernetes.io/instance: {{ .Release.Name }}
spec:
  clusterIP: None
  selector:
    app.kubernetes.io/name: scylladb
    app.kubernetes.io/instance: {{ .Release.Name }}
  ports:
    - name: cql
      port: {{ .Values.cqlPort }}
      targetPort: 9042
    - name: prometheus
      port: {{ .Values.metricsPort }}
      targetPort: 9180
YAML

cat <<'YAML' > "$TMP_SCYLLA/scylladb/templates/statefulset.yaml"
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: scylladb
  namespace: {{ .Release.Namespace }}
  labels:
    app.kubernetes.io/name: scylladb
    app.kubernetes.io/instance: {{ .Release.Name }}
spec:
  serviceName: scylladb
  replicas: {{ .Values.replicas }}
  selector:
    matchLabels:
      app.kubernetes.io/name: scylladb
      app.kubernetes.io/instance: {{ .Release.Name }}
  template:
    metadata:
      labels:
        app.kubernetes.io/name: scylladb
        app.kubernetes.io/instance: {{ .Release.Name }}
    spec:
      containers:
        - name: scylla
          image: "{{ .Values.image.repository }}:{{ .Values.image.tag }}"
          imagePullPolicy: {{ .Values.image.pullPolicy }}
          args: ["--developer-mode", "1", "--smp", "{{ .Values.smp }}", "--memory", "{{ .Values.memory }}", "--overprovisioned", "1"]
          ports:
            - name: cql
              containerPort: 9042
            - name: prometheus
              containerPort: 9180
YAML

helm --kube-context "$CTX" upgrade --install scylladb "$TMP_SCYLLA/scylladb" \
  -n scylla --create-namespace >/dev/null

echo "==> Ensuring multi-container-demo deployment (default)..."
kubectl --context "$CTX" apply -n default -f - >/dev/null <<'YAML'
apiVersion: apps/v1
kind: Deployment
metadata:
  name: multi-container-demo
  labels:
    app: multi-container-demo
spec:
  replicas: 2
  selector:
    matchLabels:
      app: multi-container-demo
  template:
    metadata:
      labels:
        app: multi-container-demo
    spec:
      containers:
        - name: web-app
          image: nginx:alpine
          ports:
            - containerPort: 80
        - name: sidecar-metrics
          image: busybox:latest
          command: ["/bin/sh", "-c", "while true; do sleep 30; done"]
YAML

echo "==> Ensuring kube-app plain-manifest & ServiceMonitor (simple-app in default)..."
if [[ -d "$KUBE_APP_ROOT/plain-manifest" ]]; then
  kubectl --context "$CTX" apply -n default -f "$KUBE_APP_ROOT/plain-manifest" >/dev/null
fi
if kubectl --context "$CTX" get crd servicemonitors.monitoring.coreos.com >/dev/null 2>&1; then
  kubectl --context "$CTX" apply -n default -f - >/dev/null <<'YAML'
apiVersion: monitoring.coreos.com/v1
kind: ServiceMonitor
metadata:
  name: simple-app
  namespace: default
  labels:
    app: simple-app
    release: prometheus
spec:
  selector:
    matchLabels:
      app: simple-app
  endpoints:
    - port: http
      path: /
      interval: 15s
YAML
fi

echo "==> Waiting for workloads to reach Ready..."
if [[ -d "$KUBE_APP_ROOT/plain-manifest" ]]; then
  kubectl --context "$CTX" rollout status deployment/simple-app -n default --timeout=90s
fi
kubectl --context "$CTX" rollout status deployment/podinfo -n default --timeout=90s
kubectl --context "$CTX" rollout status deployment/multi-container-demo -n default --timeout=90s
kubectl --context "$CTX" rollout status statefulset/redis-master -n cache --timeout=120s
kubectl --context "$CTX" rollout status statefulset/scylladb -n scylla --timeout=180s
helm --kube-context "$CTX" list -A
