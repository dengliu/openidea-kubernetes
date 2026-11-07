# Kubernetes for OpenIDEA

An [OpenIDEA](https://github.com/l0ng-ai/openidea) plugin (`dengliu.kubernetes`) that combines a [Freelens](https://github.com/freelensapp/freelens)-style Kubernetes cluster explorer with [JetBrains Kubernetes](https://plugins.jetbrains.com/plugin/10485-kubernetes)-style YAML, Helm, and Kustomize editor intelligence.

![Architecture](./architecture.excalidraw.svg)

See [design.md](./design.md) for the full architectural and protocol design.

## Features

### 1. Freelens-Style Cluster Explorer (`web-src/`)
- **Multi-Cluster Hotbar & Catalog**: Automatically discovers contexts from `~/.kube/config` and Freelens cluster store metadata (`kubeconfig-sync` / `cluster-store.json`), with 1-click hotbar switching.
- **32+ Built-in Kubernetes Resource Kinds**:
  - **Cluster & Nodes**: Cluster Overview, Nodes (with live CPU/Memory progress bars from `kubectl top nodes`, Disk, Roles, Taints, Version, Internal IP, Age, Schedulable badge, Conditions, and Node Cordon/Uncordon/Drain/Shell actions), Namespaces, Events.
  - **Workloads**: Workloads Overview, Pods (with live CPU/Memory metrics from `kubectl top pods`, container status dots, QoS, Controlled By), Deployments, DaemonSets, StatefulSets, ReplicaSets, Jobs, CronJobs (Trigger & Suspend/Resume).
  - **Config & Network & Storage**: ConfigMaps, Secrets (inline base64 decode/reveal), ResourceQuotas, LimitRanges, HPAs, PDBs, PriorityClasses, RuntimeClasses, Leases, Mutating/Validating WebhookConfigurations, Services, Endpoints, EndpointSlices, Ingresses, IngressClasses, NetworkPolicies, PVCs, PVs, StorageClasses.
  - **Access Control (RBAC)**: ServiceAccounts, ClusterRoles, Roles, ClusterRoleBindings, RoleBindings.
  - **Custom Resources (CRD & CRs)**: Groups discovered CRDs by API Group in the sidebar (`CR:<crd_name>`) to browse Custom Resource instances directly, and lists Custom Objects inside the CRD details drawer.
  - **Helm Releases**: Lists installed Helm releases and revision metadata across namespaces.
- **Interactive Details Drawer**: Comprehensive Freelens-parity inspection drawer for all 32+ resource kinds, including Node `Capacity` & `Allocatable` (`GiB` conversion), collapsible YAML `Affinities` viewer (`<YamlCodeViewer>`), sortable `Pods` table with CPU/Memory progress bars, and resource `Events`.
- **Live Pod & Node Terminal (`xterm.js`), Log Tabs, and Resource YAML Tabs**:
  - Interactive `xterm.js` PTY shell for Pod containers (`kubectl exec -it`) and Nodes (`kubectl debug node/... -it --image=busybox`).
  - Streaming Pod log Editor Tabs (`kubectl logs --follow`) with container selector and toolbar toggles (`Timestamps`, `Previous`, `Wrap`, `Follow`).
  - Live Cluster Resource YAML Editor Tabs with `Reload`, `Diff` (`kubectl diff -f -`), and `Save & Apply` (`kubectl apply -f -`).

### 2. YAML, Helm & Kustomize Editor Intelligence (`src/`)
- **Cross-Resource Navigation & Gutter Line Markers**: Navigate between `Service.spec.selector` ↔ `Deployment`/`StatefulSet`/`DaemonSet`/`Pod` labels, `envFrom`/`valueFrom` ↔ `ConfigMap`/`Secret` keys, `Ingress` ↔ backend `Service`, `PVC` ↔ `StorageClass`, and `RoleBinding` ↔ `Role`/`ServiceAccount`.
- **Context-Aware Completion & Live Templates**: Completes `apiVersion`, `kind`, target `ConfigMap`/`Secret`/`Service`/`StorageClass`/`ServiceAccount` names, data keys, and common container/probe/volume/securityContext fields, plus file templates (`Deployment`, `Service`, `ConfigMap`, `Ingress`, `Kustomization`).
- **8 Real-Time Manifest Inspections & Quick Fixes**:
  - `k8s-latest-tag`: Container image uses `:latest` or omits an explicit tag → Quick Fix pins `:1.0.0`.
  - `k8s-missing-resources`: Workload container omits `resources.limits`/`requests` → Quick Fix inserts CPU/memory requests & limits.
  - `k8s-missing-probes`: Workload container exposes ports without `livenessProbe`/`readinessProbe` → Quick Fix inserts HTTP readiness probe.
  - `k8s-privileged-container`: Container runs with `privileged: true` → Quick Fix sets `privileged: false`.
  - `k8s-missing-selector`: `Deployment`/`StatefulSet`/`DaemonSet` omits `spec.selector.matchLabels`.
  - `k8s-selector-mismatch`: Workload `spec.selector.matchLabels` does not match `spec.template.metadata.labels`.
  - `k8s-dangling-service-selector`: `Service` selector does not match any workload in the project.
  - `k8s-unresolved-ref`: Reference to a `ConfigMap`, `Secret`, `Service`, or key not found in the workspace.

## Capabilities

| Capability | Required / Optional | Why it is needed |
| --- | --- | --- |
| `process:kubectl` | Required | Queries cluster contexts, namespaces, resources, metrics (`kubectl get`, `kubectl top`, `kubectl describe`, `kubectl config view`), and executes user-initiated cluster actions (`apply`, `diff`, `delete`, `scale`, `rollout restart`, `cordon`, `uncordon`, `drain`, `port-forward`). |
| `process:helm` | Optional | Lists Helm releases (`helm list -o json`) and renders chart templates (`helm template`). |
| `process:kustomize` | Optional | Builds Kustomize overlays (`kustomize build`) when previewing or inspecting overlays. |
| `project.read` | Required | Scans workspace YAML, Helm, and Kustomize files (`*.yaml`, `*.yml`) to build the cross-resource manifest index. |
| `editor.read` | Required | Reads the active YAML editor buffer and cursor/selection for Go to Definition, completions, and manifest commands (`Apply`, `Diff`, `Dry Run`, `Delete`). |
| `editor.write` | Required | Applies quick-fix text edits to open YAML manifests. |
| `events` | Required | Subscribes to `file-opened` and `file-saved` events to re-index workspace manifests and refresh diagnostics automatically. |
| `storage` | Required | Persists the selected cluster context, namespace, `allNamespaces` toggle, and custom kubeconfig path across IDE restarts. |
| `clipboard` | Required | Copies resource names, `kubectl` commands, and rendered Helm/Kustomize output to the system clipboard. |

## Building & Packaging

```sh
pnpm --dir web-src install --frozen-lockfile
pnpm --dir web-src test
pnpm --dir web-src run build
openidea-plugin build .
openidea-plugin check .
openidea-plugin pack . --key ~/.openidea/dengliu.key --publisher dengliu
```

## License

[Apache-2.0](LICENSE)
