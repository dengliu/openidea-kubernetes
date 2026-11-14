//! Editor intelligence for Kubernetes YAML manifests, Kustomizations, and Helm charts:
//! - Project-wide resource index (cached with TTL + updated on save/open)
//! - Context-aware completion (apiVersion, kind, live templates, resource names, keys, ports, labels, Helm values)
//! - Cross-resource Go to Definition & Gutter Line Markers
//! - Static inspections & Quick Fixes (`CodeAction`)

use crate::manifest::{
    HelmActionContext, KubeDoc, ParsedFile, Spanned, helm_action_context_at, parse_yaml, utf16_len,
    yaml_context_at,
};
use openidea_plugin::providers::{
    CodeAction, CompletionItem, CompletionKind, Document, LineMarker,
};
use openidea_plugin::{Diagnostic, Location, Position, Project, Range, Severity, TextEdit, host};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

const INDEX_TTL: Duration = Duration::from_secs(300);
const MAX_YAML_FILES: u32 = 500;
const MAX_MARKER_FILES: u32 = 500;
const MAX_INDEX_FILE_BYTES: usize = 256 << 10;
const MAX_INDEX_SCAN_TIME: Duration = Duration::from_millis(600);

/// Icon ids declared under `[[contributes.fileIcons]]` in `plugin.toml`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KubeFileIcon {
    Kubernetes,
    HelmChartDir,
    HelmChart,
    HelmValues,
    HelmTemplate,
    KustomizeDir,
    Kustomize,
    KustomizePatch,
}

impl KubeFileIcon {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Kubernetes => "kubernetes",
            Self::HelmChartDir => "helm-chart-dir",
            Self::HelmChart => "helm-chart",
            Self::HelmValues => "helm-values",
            Self::HelmTemplate => "helm-template",
            Self::KustomizeDir => "kustomize-dir",
            Self::Kustomize => "kustomize",
            Self::KustomizePatch => "kustomize-patch",
        }
    }

    pub const fn source_kind(self) -> &'static str {
        match self {
            Self::Kubernetes => "kubernetes",
            Self::HelmChartDir | Self::HelmChart | Self::HelmValues | Self::HelmTemplate => "helm",
            Self::KustomizeDir | Self::Kustomize | Self::KustomizePatch => "kustomize",
        }
    }
}

/// Returns true if `path` is inside an excluded build/dependency/CI directory.
pub fn is_excluded_workspace_dir(path: &str) -> bool {
    let norm = normalize_slashes(path);
    norm.contains("/node_modules/")
        || norm.contains("/.git/")
        || norm.contains("/.github/workflows/")
        || norm.contains("/.gitlab/")
        || norm.contains("/.circleci/")
        || norm.contains("/target/")
        || norm.contains("/dist/")
        || norm.contains("/vendor/")
        || norm.contains("/.venv/")
        || norm.contains("/venv/")
}

/// Returns true if `path` is a lockfile, dependency folder, or CI config that should not be indexed as a Kubernetes manifest.
pub fn is_excluded_yaml_path(path: &str) -> bool {
    let norm = normalize_slashes(path);
    let fname = norm.rsplit('/').next().unwrap_or(&norm);
    if matches!(
        fname,
        "pnpm-lock.yaml"
            | "yarn.lock"
            | "package-lock.json"
            | "Chart.lock"
            | "requirements.lock"
            | "pubspec.lock"
            | "docker-compose.yml"
            | "docker-compose.yaml"
            | "compose.yml"
            | "compose.yaml"
            | ".clang-format"
            | ".clang-tidy"
            | ". prettierrc.yaml"
            | ".prettierrc.yml"
    ) || fname.ends_with("-lock.yaml")
        || fname.ends_with(".lock.yaml")
    {
        return true;
    }
    is_excluded_workspace_dir(&norm)
}

pub fn is_yaml_filename(fname: &str) -> bool {
    fname.ends_with(".yaml") || fname.ends_with(".yml")
}

pub fn is_helm_values_filename(fname: &str) -> bool {
    fname == "values.yaml"
        || fname == "values.yml"
        || fname == "values.schema.json"
        || ((fname.starts_with("values.")
            || fname.starts_with("values-")
            || fname.starts_with("values_"))
            && is_yaml_filename(fname))
        || fname.ends_with(".values.yaml")
        || fname.ends_with(".values.yml")
}

/// Fast check before running the full line-by-line parser during background workspace scans.
fn is_likely_kube_or_helm_file(path: &str, text: &str) -> bool {
    if text.is_empty() || text.len() > MAX_INDEX_FILE_BYTES {
        return false;
    }
    let norm = normalize_slashes(path);
    let fname = norm.rsplit('/').next().unwrap_or(&norm);
    if is_helm_values_filename(fname)
        || matches!(
            fname,
            "Chart.yaml"
                | "Chart.yml"
                | "requirements.yaml"
                | "requirements.yml"
                | "helmfile.yaml"
                | "helmfile.yml"
                | "kustomization.yaml"
                | "kustomization.yml"
                | "Kustomization"
        )
        || fname.ends_with(".helmfile.yaml")
        || fname.ends_with(".helmfile.yml")
        || fname.ends_with(".kustomize.yaml")
        || fname.ends_with(".kustomize.yml")
        || fname.ends_with(".k8s.yaml")
        || fname.ends_with(".k8s.yml")
        || fname.ends_with(".helm.yaml")
        || fname.ends_with(".helm.yml")
        || fname.ends_with(".tpl")
    {
        return true;
    }
    text.contains("apiVersion:")
        || text.contains("kind:")
        || text.contains(".Values.")
        || text.contains("define \"")
        || text.contains("{{")
        || text.contains("resources:")
        || text.contains("patches:")
        || text.contains("patchesStrategicMerge:")
        || text.contains("patchesJson6902:")
        || text.contains("components:")
        || text.contains("bases:")
        || text.contains("- op:")
}

/// Project-wide index of parsed YAML manifests, Helm charts, and Kustomize overlays.
#[derive(Clone, Debug, Default)]
pub struct ProjectIndex {
    pub built_at: Option<Instant>,
    pub files: BTreeMap<String, ParsedFile>,
    /// Directories containing `Chart.yaml` or `Chart.yml`.
    pub helm_chart_dirs: BTreeSet<String>,
    /// Directories containing `kustomization.yaml`, `kustomization.yml`, or `Kustomization`.
    pub kustomize_dirs: BTreeSet<String>,
    /// Non-YAML or auxiliary Helm/Kustomize files (`NOTES.txt`, `.helmignore`, `Chart.lock`, `values.schema.json`, JSON patches).
    pub extra_files: BTreeSet<String>,
}

/// One diagnosed issue and its optional quick fix.
#[derive(Clone, Debug)]
pub struct InspectionIssue {
    pub diagnostic: Diagnostic,
    pub fix: Option<CodeAction>,
}

pub const DEPRECATED_APIS: &[(&str, &str, &str)] = &[
    ("extensions/v1beta1", "Deployment", "apps/v1"),
    ("apps/v1beta1", "Deployment", "apps/v1"),
    ("apps/v1beta2", "Deployment", "apps/v1"),
    ("apps/v1beta1", "StatefulSet", "apps/v1"),
    ("apps/v1beta2", "StatefulSet", "apps/v1"),
    ("extensions/v1beta1", "DaemonSet", "apps/v1"),
    ("apps/v1beta2", "DaemonSet", "apps/v1"),
    ("extensions/v1beta1", "ReplicaSet", "apps/v1"),
    ("apps/v1beta2", "ReplicaSet", "apps/v1"),
    ("extensions/v1beta1", "Ingress", "networking.k8s.io/v1"),
    (
        "networking.k8s.io/v1beta1",
        "Ingress",
        "networking.k8s.io/v1",
    ),
    (
        "extensions/v1beta1",
        "NetworkPolicy",
        "networking.k8s.io/v1",
    ),
    ("batch/v1beta1", "CronJob", "batch/v1"),
    ("policy/v1beta1", "PodDisruptionBudget", "policy/v1"),
    (
        "autoscaling/v2beta1",
        "HorizontalPodAutoscaler",
        "autoscaling/v2",
    ),
    (
        "autoscaling/v2beta2",
        "HorizontalPodAutoscaler",
        "autoscaling/v2",
    ),
    (
        "rbac.authorization.k8s.io/v1beta1",
        "Role",
        "rbac.authorization.k8s.io/v1",
    ),
    (
        "rbac.authorization.k8s.io/v1beta1",
        "ClusterRole",
        "rbac.authorization.k8s.io/v1",
    ),
    (
        "rbac.authorization.k8s.io/v1beta1",
        "RoleBinding",
        "rbac.authorization.k8s.io/v1",
    ),
    (
        "rbac.authorization.k8s.io/v1beta1",
        "ClusterRoleBinding",
        "rbac.authorization.k8s.io/v1",
    ),
    (
        "storage.k8s.io/v1beta1",
        "StorageClass",
        "storage.k8s.io/v1",
    ),
    (
        "apiextensions.k8s.io/v1beta1",
        "CustomResourceDefinition",
        "apiextensions.k8s.io/v1",
    ),
];

pub const API_VERSIONS: &[(&str, &str)] = &[
    (
        "v1",
        "Core Kubernetes API (Pod, Service, ConfigMap, Secret, Namespace, PVC)",
    ),
    (
        "apps/v1",
        "Workloads API (Deployment, StatefulSet, DaemonSet, ReplicaSet)",
    ),
    ("batch/v1", "Batch API (Job, CronJob)"),
    (
        "networking.k8s.io/v1",
        "Networking API (Ingress, NetworkPolicy, IngressClass)",
    ),
    (
        "rbac.authorization.k8s.io/v1",
        "RBAC API (Role, ClusterRole, RoleBinding, ClusterRoleBinding)",
    ),
    (
        "autoscaling/v2",
        "Autoscaling API (HorizontalPodAutoscaler)",
    ),
    ("policy/v1", "Policy API (PodDisruptionBudget)"),
    (
        "storage.k8s.io/v1",
        "Storage API (StorageClass, VolumeAttachment, CSIDriver)",
    ),
    (
        "apiextensions.k8s.io/v1",
        "CRD API (CustomResourceDefinition)",
    ),
    ("kustomize.config.k8s.io/v1beta1", "Kustomize configuration"),
];

pub const KINDS: &[(&str, &str)] = &[
    ("Deployment", "apps/v1 — Managed stateless workload"),
    ("Service", "v1 — Network service exposing Pods"),
    ("ConfigMap", "v1 — Non-confidential configuration data"),
    ("Secret", "v1 — Sensitive configuration data"),
    ("Pod", "v1 — Smallest deployable compute unit"),
    (
        "StatefulSet",
        "apps/v1 — Stateful workload with stable identity",
    ),
    ("DaemonSet", "apps/v1 — Node-local background workload"),
    ("Job", "batch/v1 — Run-to-completion task"),
    ("CronJob", "batch/v1 — Scheduled Job"),
    (
        "Ingress",
        "networking.k8s.io/v1 — HTTP/HTTPS routing to Services",
    ),
    (
        "NetworkPolicy",
        "networking.k8s.io/v1 — Pod network isolation policy",
    ),
    (
        "PersistentVolumeClaim",
        "v1 — Storage request bound to a PV",
    ),
    ("PersistentVolume", "v1 — Cluster storage volume"),
    (
        "ServiceAccount",
        "v1 — Pod identity for cluster authentication",
    ),
    (
        "Role",
        "rbac.authorization.k8s.io/v1 — Namespaced RBAC permissions",
    ),
    (
        "ClusterRole",
        "rbac.authorization.k8s.io/v1 — Cluster-wide RBAC permissions",
    ),
    (
        "RoleBinding",
        "rbac.authorization.k8s.io/v1 — Binds a Role to subjects",
    ),
    (
        "ClusterRoleBinding",
        "rbac.authorization.k8s.io/v1 — Binds a ClusterRole cluster-wide",
    ),
    ("Namespace", "v1 — Virtual cluster partition"),
    (
        "HorizontalPodAutoscaler",
        "autoscaling/v2 — Automatic replica scaling",
    ),
    (
        "PodDisruptionBudget",
        "policy/v1 — Voluntary disruption budget",
    ),
    (
        "StorageClass",
        "storage.k8s.io/v1 — Dynamic volume provisioning profile",
    ),
    (
        "Kustomization",
        "kustomize.config.k8s.io/v1beta1 — Kustomize overlay manifest",
    ),
];

pub fn project_of(path: &str) -> Option<Project> {
    let norm = normalize_slashes(path);
    host::projects()
        .ok()?
        .into_iter()
        .filter(|p| {
            p.root()
                .is_ok_and(|r| norm.starts_with(&normalize_slashes(&r)))
        })
        .max_by_key(|p| p.root().map(|r| r.len()).unwrap_or(0))
}

pub fn normalize_slashes(path: &str) -> String {
    path.replace('\\', "/")
}

pub fn parent_dir(path: &str) -> String {
    let norm = normalize_slashes(path);
    match norm.rfind('/') {
        Some(0) => "/".into(),
        Some(i) => norm[..i].to_owned(),
        None => String::new(),
    }
}

pub fn join_relative(base_dir: &str, rel: &str) -> String {
    let rel_norm = normalize_slashes(rel);
    let mut parts: Vec<&str> = base_dir.split('/').filter(|s| !s.is_empty()).collect();
    let leading_slash = base_dir.starts_with('/');
    for seg in rel_norm.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    if leading_slash {
        format!("/{}", parts.join("/"))
    } else {
        parts.join("/")
    }
}

impl ProjectIndex {
    /// Clears TTL so the next `ensure` performs a fresh project scan.
    pub fn invalidate(&mut self) {
        self.built_at = None;
    }

    fn record_special_marker_path(&mut self, norm: &str) {
        let fname = norm.rsplit('/').next().unwrap_or(norm);
        if matches!(fname, "Chart.yaml" | "Chart.yml") {
            let dir = parent_dir(norm);
            if !dir.is_empty() {
                self.helm_chart_dirs.insert(dir);
            }
        } else if matches!(
            fname,
            "kustomization.yaml" | "kustomization.yml" | "Kustomization"
        ) {
            let dir = parent_dir(norm);
            if !dir.is_empty() {
                self.kustomize_dirs.insert(dir);
            }
        }
    }

    /// Builds or refreshes the YAML index for `project`, always overlaying `current_doc` if provided.
    pub fn ensure(&mut self, project: Option<&Project>, current_doc: Option<(&str, &str)>) {
        let fresh = self.built_at.is_some_and(|t| t.elapsed() < INDEX_TTL);
        if !fresh && let Some(proj) = project {
            let started = Instant::now();
            let proj_root = proj
                .root()
                .ok()
                .map(|r| normalize_slashes(&r).trim_end_matches('/').to_owned());
            let belongs_to_proj = |p: &str| {
                proj_root
                    .as_ref()
                    .is_some_and(|r| p == r || p.starts_with(&format!("{r}/")))
            };

            let mut files = std::mem::take(&mut self.files);
            files.retain(|k, _| !belongs_to_proj(k));
            self.helm_chart_dirs.retain(|d| !belongs_to_proj(d));
            self.kustomize_dirs.retain(|d| !belongs_to_proj(d));
            self.extra_files.retain(|p| !belongs_to_proj(p));

            // 1. Always discover Helm chart roots (`Chart.yaml`), Kustomize roots (`kustomization.yaml`),
            // and auxiliary Helm files first so chart/overlay directory context is complete even in large repos.
            for pat in [
                "**/Chart.yaml",
                "**/Chart.yml",
                "**/kustomization.yaml",
                "**/kustomization.yml",
                "**/Kustomization",
            ] {
                if let Ok(paths) = proj.find_files(pat, MAX_MARKER_FILES) {
                    for p in paths {
                        if is_excluded_workspace_dir(&p) {
                            continue;
                        }
                        let norm = normalize_slashes(&p);
                        self.record_special_marker_path(&norm);
                        if let Ok(text) = proj.read_file(&p)
                            && text.len() <= MAX_INDEX_FILE_BYTES
                        {
                            files.insert(norm, parse_yaml(&text));
                        }
                    }
                }
            }

            for pat in [
                "**/Chart.lock",
                "**/requirements.lock",
                "**/.helmignore",
                "**/NOTES.txt",
                "**/values.schema.json",
                "**/*.kubeconfig",
                "**/kubeconfig",
            ] {
                if let Ok(paths) = proj.find_files(pat, MAX_MARKER_FILES) {
                    for p in paths {
                        if !is_excluded_workspace_dir(&p) {
                            self.extra_files.insert(normalize_slashes(&p));
                        }
                    }
                }
            }

            // 2. Index YAML and `.tpl` files across the workspace (including files inside Helm `templates/`).
            'outer: for pat in ["**/*.yaml", "**/*.yml", "**/*.tpl"] {
                if started.elapsed() >= MAX_INDEX_SCAN_TIME {
                    break;
                }
                if let Ok(paths) = proj.find_files(pat, MAX_YAML_FILES) {
                    for p in paths {
                        if started.elapsed() >= MAX_INDEX_SCAN_TIME {
                            break 'outer;
                        }
                        if is_excluded_yaml_path(&p) {
                            continue;
                        }
                        let norm = normalize_slashes(&p);
                        if files.contains_key(&norm) {
                            continue;
                        }
                        let dir = parent_dir(&norm);
                        let in_helm_chart = self
                            .helm_chart_dirs
                            .iter()
                            .any(|c| dir == *c || dir.starts_with(&format!("{c}/")));
                        let in_kustomize_dir = self
                            .kustomize_dirs
                            .iter()
                            .any(|k| dir == *k || dir.starts_with(&format!("{k}/")));
                        if norm.ends_with(".tpl") {
                            self.extra_files.insert(norm.clone());
                        }
                        if let Ok(text) = proj.read_file(&p) {
                            if !in_helm_chart
                                && !in_kustomize_dir
                                && !is_likely_kube_or_helm_file(&norm, &text)
                            {
                                continue;
                            }
                            if text.len() > MAX_INDEX_FILE_BYTES {
                                continue;
                            }
                            let parsed = parse_yaml(&text);
                            if in_helm_chart
                                || in_kustomize_dir
                                || !parsed.docs.is_empty()
                                || !parsed.helm_values.is_empty()
                                || !parsed.helm_value_refs.is_empty()
                                || !parsed.helm_template_defs.is_empty()
                                || !parsed.helm_template_refs.is_empty()
                                || parsed.has_helm_directives
                                || parsed.has_json6902_ops
                            {
                                files.insert(norm, parsed);
                            }
                        }
                    }
                }
            }
            self.files = files;
            self.built_at = Some(Instant::now());
        }
        if let Some((path, text)) = current_doc
            && !is_excluded_yaml_path(path)
            && text.len() <= MAX_INDEX_FILE_BYTES
        {
            let norm = normalize_slashes(path);
            self.record_special_marker_path(&norm);
            self.files.insert(norm.clone(), parse_yaml(text));
            // Also opportunistically index adjacent Helm chart files (`../values.yaml`, `_helpers.tpl`) by direct path
            if let Some(proj) = project {
                let dir = parent_dir(&norm);
                let chart_root = if dir.ends_with("/templates") {
                    parent_dir(&dir)
                } else {
                    dir.clone()
                };
                for chart_marker in [
                    format!("{chart_root}/Chart.yaml"),
                    format!("{chart_root}/Chart.yml"),
                ] {
                    if !self.files.contains_key(&chart_marker)
                        && let Ok(ctext) = proj.read_file(&chart_marker)
                        && ctext.len() <= MAX_INDEX_FILE_BYTES
                    {
                        self.record_special_marker_path(&chart_marker);
                        self.files.insert(chart_marker, parse_yaml(&ctext));
                    }
                }
                for candidate in [
                    format!("{chart_root}/values.yaml"),
                    format!("{chart_root}/values.yml"),
                    format!("{dir}/_helpers.tpl"),
                ] {
                    if !self.files.contains_key(&candidate)
                        && let Ok(ctext) = proj.read_file(&candidate)
                        && ctext.len() <= MAX_INDEX_FILE_BYTES
                    {
                        self.files.insert(candidate, parse_yaml(&ctext));
                    }
                }
            }
        }
    }

    pub fn update_file(&mut self, path: &str, text: &str) {
        let norm = normalize_slashes(path);
        self.record_special_marker_path(&norm);
        self.files.insert(norm, parse_yaml(text));
    }

    /// Deepest Helm chart root directory (`Chart.yaml` / `Chart.yml`) containing `path`, if any.
    pub fn enclosing_helm_chart_dir<'a>(&'a self, path: &str) -> Option<&'a str> {
        let dir = parent_dir(path);
        self.helm_chart_dirs
            .iter()
            .filter(|c| dir == **c || dir.starts_with(&format!("{c}/")))
            .max_by_key(|c| c.len())
            .map(String::as_str)
    }

    fn is_kustomization_root_file(path: &str, parsed: Option<&ParsedFile>) -> bool {
        let fname = path.rsplit('/').next().unwrap_or(path);
        if matches!(
            fname,
            "kustomization.yaml" | "kustomization.yml" | "Kustomization"
        ) {
            return true;
        }
        parsed.is_some_and(|p| {
            !p.marked_non_k8s
                && p.docs.iter().any(|d| {
                    d.api_version
                        .as_ref()
                        .is_some_and(|a| a.value.starts_with("kustomize.config.k8s.io/"))
                        && d.kind.as_ref().is_some_and(|k| {
                            matches!(k.value.as_str(), "Kustomization" | "Component")
                        })
                })
        })
    }

    fn collect_kustomize_targets(&self) -> (BTreeSet<String>, BTreeSet<String>, BTreeSet<String>) {
        let mut patch_files = BTreeSet::new();
        let mut helm_value_files = BTreeSet::new();
        let mut component_dirs = BTreeSet::new();
        let is_remote = |v: &str| {
            v.starts_with("http://")
                || v.starts_with("https://")
                || v.starts_with("ssh://")
                || v.starts_with("git::")
                || v.starts_with("oci://")
        };
        for (kpath, parsed) in &self.files {
            if !Self::is_kustomization_root_file(kpath, Some(parsed)) {
                continue;
            }
            let base = parent_dir(kpath);
            for doc in &parsed.docs {
                for sp in &doc.kustomize_patch_paths {
                    if !is_remote(&sp.value) {
                        patch_files.insert(join_relative(&base, &sp.value));
                    }
                }
                for sp in &doc.kustomize_helm_values {
                    if !is_remote(&sp.value) {
                        helm_value_files.insert(join_relative(&base, &sp.value));
                    }
                }
                for sp in &doc.kustomize_component_dirs {
                    if !is_remote(&sp.value) {
                        let resolved = join_relative(&base, &sp.value);
                        if !self.files.contains_key(&resolved) {
                            component_dirs.insert(resolved);
                        }
                    }
                }
            }
        }
        (patch_files, helm_value_files, component_dirs)
    }

    /// Classifies a single file path into its Kubernetes / Helm / Kustomize icon kind.
    pub fn classify_file_icon(&self, path: &str) -> Option<KubeFileIcon> {
        let (patch_files, helm_value_files, component_dirs) = self.collect_kustomize_targets();
        let norm = normalize_slashes(path);
        self.classify_file_icon_with(
            &norm,
            self.files.get(&norm),
            &patch_files,
            &helm_value_files,
            &component_dirs,
        )
    }

    fn classify_file_icon_with(
        &self,
        norm: &str,
        parsed: Option<&ParsedFile>,
        kustomize_patch_files: &BTreeSet<String>,
        kustomize_helm_value_files: &BTreeSet<String>,
        kustomize_component_dirs: &BTreeSet<String>,
    ) -> Option<KubeFileIcon> {
        if is_excluded_workspace_dir(norm) {
            return None;
        }
        let fname = norm.rsplit('/').next().unwrap_or(norm);
        if matches!(
            fname,
            "pnpm-lock.yaml"
                | "yarn.lock"
                | "package-lock.json"
                | "pubspec.lock"
                | "docker-compose.yml"
                | "docker-compose.yaml"
                | "compose.yml"
                | "compose.yaml"
        ) || fname.ends_with("-lock.yaml")
            || fname.ends_with(".lock.yaml")
        {
            return None;
        }

        let dir = parent_dir(norm);

        // 1. Inside a Helm chart directory (`Chart.yaml` / `Chart.yml`)
        if let Some(chart_root) = self.enclosing_helm_chart_dir(norm) {
            if dir == chart_root
                && matches!(
                    fname,
                    "Chart.yaml"
                        | "Chart.yml"
                        | "Chart.lock"
                        | "requirements.yaml"
                        | "requirements.yml"
                        | "requirements.lock"
                )
            {
                return Some(KubeFileIcon::HelmChart);
            }
            if fname == ".helmignore" || fname.ends_with(".tpl") {
                return Some(KubeFileIcon::HelmTemplate);
            }
            let templates_dir = format!("{chart_root}/templates");
            if dir == templates_dir || dir.starts_with(&format!("{templates_dir}/")) {
                if is_yaml_filename(fname)
                    || fname.ends_with(".tpl")
                    || fname.ends_with(".json")
                    || fname == "NOTES.txt"
                {
                    return Some(KubeFileIcon::HelmTemplate);
                }
                return None;
            }
            if is_helm_values_filename(fname)
                || (dir == format!("{chart_root}/ci") && is_yaml_filename(fname))
                || kustomize_helm_value_files.contains(norm)
            {
                return Some(KubeFileIcon::HelmValues);
            }
            if parsed.is_some_and(|p| !p.marked_non_k8s && p.has_helm_directives) {
                return Some(KubeFileIcon::HelmTemplate);
            }
        }

        // 2. Standalone Helm files (outside a `Chart.yaml` directory, e.g. `helmfile.yaml`, `*.tpl`, `values.yaml`)
        if matches!(
            fname,
            "Chart.yaml"
                | "Chart.yml"
                | "Chart.lock"
                | "requirements.lock"
                | "helmfile.yaml"
                | "helmfile.yml"
        ) || fname.ends_with(".helmfile.yaml")
            || fname.ends_with(".helmfile.yml")
            || (dir.ends_with("/helmfile.d") && is_yaml_filename(fname))
        {
            return Some(KubeFileIcon::HelmChart);
        }
        if fname == ".helmignore"
            || fname.ends_with(".tpl")
            || fname.ends_with(".helm.yaml")
            || fname.ends_with(".helm.yml")
        {
            return Some(KubeFileIcon::HelmTemplate);
        }
        if is_helm_values_filename(fname) || kustomize_helm_value_files.contains(norm) {
            return Some(KubeFileIcon::HelmValues);
        }
        if parsed.is_some_and(|p| !p.marked_non_k8s && p.has_helm_directives) {
            return Some(KubeFileIcon::HelmTemplate);
        }

        // 3. Kustomize root, component, patch, generator, transformer, and replacement files
        if Self::is_kustomization_root_file(norm, parsed) {
            return Some(KubeFileIcon::Kustomize);
        }
        if kustomize_patch_files.contains(norm)
            && (is_yaml_filename(fname) || fname.ends_with(".json"))
        {
            return Some(KubeFileIcon::KustomizePatch);
        }
        if fname.ends_with(".kustomize.yaml") || fname.ends_with(".kustomize.yml") {
            return Some(KubeFileIcon::KustomizePatch);
        }
        if let Some(p) = parsed.filter(|p| !p.marked_non_k8s) {
            if p.docs.iter().any(|d| {
                d.api_version.as_ref().is_some_and(|a| {
                    a.value == "builtin"
                        || a.value.ends_with(".kustomize.config.k8s.io")
                        || a.value.contains(".kustomize.config.k8s.io/")
                })
            }) {
                return Some(KubeFileIcon::KustomizePatch);
            }
            let in_kustomize_tree = self
                .kustomize_dirs
                .iter()
                .chain(kustomize_component_dirs.iter())
                .any(|kd| dir == *kd || dir.starts_with(&format!("{kd}/")));
            if in_kustomize_tree && p.has_json6902_ops {
                return Some(KubeFileIcon::KustomizePatch);
            }
        }

        // 4. Standard Kubernetes resource manifests (`apiVersion` + `kind`), `*.k8s.yaml`, and `kubeconfig`
        if fname.ends_with(".k8s.yaml")
            || fname.ends_with(".k8s.yml")
            || fname == "kubeconfig"
            || fname.ends_with(".kubeconfig")
        {
            return Some(KubeFileIcon::Kubernetes);
        }
        if let Some(p) = parsed.filter(|p| !p.marked_non_k8s)
            && p.docs.iter().any(|d| {
                d.api_version
                    .as_ref()
                    .is_some_and(|a| !a.value.is_empty() && !a.value.starts_with("backstage.io/"))
                    && d.kind.as_ref().is_some_and(|k| !k.value.is_empty())
            })
        {
            return Some(KubeFileIcon::Kubernetes);
        }

        None
    }

    /// Computes all `(path, KubeFileIcon)` folder and file entries for `project_root` (or all indexed paths when `None`).
    pub fn compute_workspace_icons(
        &self,
        project_root: Option<&str>,
    ) -> Vec<(String, KubeFileIcon)> {
        let norm_root = project_root.map(|r| normalize_slashes(r).trim_end_matches('/').to_owned());
        let in_scope = |p: &str| match norm_root.as_deref() {
            Some(r) if !r.is_empty() => p == r || p.starts_with(&format!("{r}/")),
            _ => true,
        };

        let (patch_files, helm_value_files, component_dirs) = self.collect_kustomize_targets();
        let mut out: BTreeMap<String, KubeFileIcon> = BTreeMap::new();

        for dir in &self.helm_chart_dirs {
            if in_scope(dir) {
                out.insert(dir.clone(), KubeFileIcon::HelmChartDir);
            }
        }
        for dir in self.kustomize_dirs.iter().chain(component_dirs.iter()) {
            if in_scope(dir) && !self.helm_chart_dirs.contains(dir) {
                out.insert(dir.clone(), KubeFileIcon::KustomizeDir);
            }
        }

        let mut all_paths: BTreeSet<&str> = self.files.keys().map(String::as_str).collect();
        for extra in &self.extra_files {
            all_paths.insert(extra.as_str());
        }
        for p in patch_files.iter().chain(helm_value_files.iter()) {
            all_paths.insert(p.as_str());
        }

        for path in all_paths {
            if !in_scope(path) {
                continue;
            }
            if let Some(icon) = self.classify_file_icon_with(
                path,
                self.files.get(path),
                &patch_files,
                &helm_value_files,
                &component_dirs,
            ) {
                out.insert(path.to_owned(), icon);
            }
        }

        out.into_iter().collect()
    }

    /// Iterates over every `(file_path, &KubeDoc)` in the workspace.
    pub fn all_docs(&self) -> impl Iterator<Item = (&str, &KubeDoc)> {
        self.files
            .iter()
            .flat_map(|(path, parsed)| parsed.docs.iter().map(move |d| (path.as_str(), d)))
    }

    /// Finds resources of `kind` and `name`.
    pub fn find_by_kind_and_name(&self, kind: &str, name: &str) -> Vec<(String, &KubeDoc, Range)> {
        let mut out = Vec::new();
        for (path, doc) in self.all_docs() {
            if doc.kind.as_ref().is_some_and(|k| k.value == kind)
                && let Some(ref n) = doc.name
                && n.value == name
            {
                out.push((path.to_owned(), doc, n.range));
            }
        }
        out
    }

    /// Finds workloads whose pod/resource labels match all `selector_labels`.
    pub fn find_matching_workloads(
        &self,
        selector_labels: &BTreeMap<String, Spanned>,
    ) -> Vec<(String, &KubeDoc, Range)> {
        if selector_labels.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (path, doc) in self.all_docs() {
            if !doc.is_workload() {
                continue;
            }
            let target = doc.target_labels();
            if selector_labels
                .iter()
                .all(|(k, v)| target.get(k).is_some_and(|tv| tv.value == v.value))
            {
                let range = doc
                    .name
                    .as_ref()
                    .map(|n| n.range)
                    .or_else(|| doc.kind.as_ref().map(|k| k.range))
                    .unwrap_or_default();
                out.push((path.to_owned(), doc, range));
            }
        }
        out
    }

    /// Finds Services whose `selector_labels` match `workload`'s target labels.
    pub fn find_matching_services(&self, workload: &KubeDoc) -> Vec<(String, &KubeDoc, Range)> {
        let target = workload.target_labels();
        if target.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (path, doc) in self.all_docs() {
            if doc.kind.as_ref().map(|k| k.value.as_str()) != Some("Service")
                || doc.selector_labels.is_empty()
            {
                continue;
            }
            if doc
                .selector_labels
                .iter()
                .all(|(k, v)| target.get(k).is_some_and(|tv| tv.value == v.value))
            {
                let range = doc
                    .name
                    .as_ref()
                    .map(|n| n.range)
                    .or_else(|| doc.kind.as_ref().map(|k| k.range))
                    .unwrap_or_default();
                out.push((path.to_owned(), doc, range));
            }
        }
        out
    }

    /// Infers the enclosing Helm chart root directory for `path`, falling back to `.../templates/..`
    /// or a values file's parent directory when `Chart.yaml` has not been scanned yet.
    pub fn infer_helm_chart_dir(&self, path: &str) -> Option<String> {
        let norm = normalize_slashes(path);
        if let Some(chart_dir) = self.enclosing_helm_chart_dir(&norm) {
            return Some(chart_dir.to_owned());
        }
        if let Some((before_tpl, _)) = norm.rsplit_once("/templates/")
            && !before_tpl.is_empty()
        {
            return Some(before_tpl.to_owned());
        }
        let fname = norm.rsplit('/').next().unwrap_or(&norm);
        if is_helm_values_filename(fname) || matches!(fname, "Chart.yaml" | "Chart.yml") {
            let p = parent_dir(&norm);
            if !p.is_empty() {
                return Some(p);
            }
        }
        None
    }

    /// Finds `values.yaml` definitions for a Helm `.Values.<key>` reference, preferring the same chart.
    pub fn find_helm_value_for(
        &self,
        doc_path: Option<&str>,
        key_path: &str,
    ) -> Vec<(String, Range, String)> {
        let chart_root = doc_path.and_then(|p| self.infer_helm_chart_dir(p));
        let mut local = Vec::new();
        let mut fallback = Vec::new();
        for (path, parsed) in &self.files {
            let file_name = path.rsplit('/').next().unwrap_or(path);
            if is_helm_values_filename(file_name)
                && let Some(sp) = parsed.helm_values.get(key_path)
            {
                let entry = (path.clone(), sp.range, sp.value.clone());
                if chart_root.as_ref().is_some_and(|root| {
                    let dir = parent_dir(path);
                    dir == *root || dir.starts_with(&format!("{root}/"))
                }) {
                    local.push(entry);
                } else {
                    fallback.push(entry);
                }
            }
        }
        if !local.is_empty() { local } else { fallback }
    }

    /// Finds `values.yaml` definitions for a Helm `.Values.<key>` reference across the workspace.
    pub fn find_helm_value(&self, key_path: &str) -> Vec<(String, Range, String)> {
        self.find_helm_value_for(None, key_path)
    }

    /// Finds `{{- define "<name>" -}}` definitions across Helm templates and `_helpers.tpl`, preferring the same chart.
    pub fn find_helm_template_for(
        &self,
        doc_path: Option<&str>,
        name: &str,
    ) -> Vec<(String, Range)> {
        let chart_root = doc_path.and_then(|p| self.infer_helm_chart_dir(p));
        let mut local = Vec::new();
        let mut fallback = Vec::new();
        for (path, parsed) in &self.files {
            if let Some(sp) = parsed.helm_template_defs.get(name) {
                if chart_root.as_ref().is_some_and(|root| {
                    let dir = parent_dir(path);
                    dir == *root || dir.starts_with(&format!("{root}/"))
                }) {
                    local.push((path.clone(), sp.range));
                } else {
                    fallback.push((path.clone(), sp.range));
                }
            }
        }
        if !local.is_empty() { local } else { fallback }
    }

    /// Finds `{{- define "<name>" -}}` definitions across Helm templates and `_helpers.tpl`.
    pub fn find_helm_template(&self, name: &str) -> Vec<(String, Range)> {
        self.find_helm_template_for(None, name)
    }

    /// Resolves a Kustomize relative path (`patch-deployment.yaml` or directory `../../base`) to an indexed file.
    pub fn resolve_kustomize_target(&self, base_dir: &str, rel_path: &str) -> Option<String> {
        let resolved = join_relative(base_dir, rel_path);
        if self.files.contains_key(&resolved) {
            return Some(resolved);
        }
        let trimmed = resolved.trim_end_matches('/');
        for kname in ["kustomization.yaml", "kustomization.yml", "Kustomization"] {
            let candidate = format!("{trimmed}/{kname}");
            if self.files.contains_key(&candidate) {
                return Some(candidate);
            }
        }
        None
    }

    /// Returns whether `path` is referenced as a Kustomize patch / transformer / generator / component.
    pub fn is_kustomize_patch_path(&self, path: &str) -> bool {
        let norm = normalize_slashes(path);
        let (patch_files, _, _) = self.collect_kustomize_targets();
        patch_files.contains(&norm)
    }

    /// Finds CRD definitions matching `kind`.
    pub fn find_crd(&self, kind: &str) -> Vec<(String, &crate::manifest::CrdSpec, Range)> {
        let mut out = Vec::new();
        for (path, doc) in self.all_docs() {
            if let Some(ref crd) = doc.crd
                && crd.kind.value == kind
            {
                out.push((path.to_owned(), crd, crd.kind.range));
            }
        }
        out
    }
}

fn contains(range: Range, pos: Position) -> bool {
    range.start <= pos && pos <= range.end
}

/// Computes navigation links `(source_range, target_location, tooltip, icon)` for `doc`.
pub fn document_links(
    index: &ProjectIndex,
    doc_path: &str,
    parsed: &ParsedFile,
) -> Vec<(Range, Location, String, &'static str)> {
    let mut links = Vec::new();
    let norm_path = normalize_slashes(doc_path);
    let base_dir = parent_dir(&norm_path);

    let fname = norm_path.rsplit('/').next().unwrap_or(&norm_path);

    // 1. Helm `.Values.xxx` / scoped `.field` / `$var.field` references -> `values.yaml`
    for href in &parsed.helm_value_refs {
        for (vpath, vrange, vval) in index.find_helm_value_for(Some(&norm_path), &href.value) {
            let vfname = vpath.rsplit('/').next().unwrap_or(&vpath);
            let detail = if vval.is_empty() {
                format!("{vfname}: {}", href.value)
            } else {
                format!("{vfname}: {} = {}", href.value, vval)
            };
            links.push((
                href.range,
                Location {
                    path: vpath,
                    range: vrange,
                },
                format!("Go to Helm value ({detail})"),
                "navigate",
            ));
        }
    }

    // 1a. Helm `$var` references -> `$var := ...` definition in the template
    for (var_name, var_range) in &parsed.helm_var_refs {
        if let Some((def_sp, _)) = parsed.helm_var_defs.get(var_name) {
            links.push((
                *var_range,
                Location {
                    path: norm_path.clone(),
                    range: def_sp.range,
                },
                format!("Go to Helm variable '{var_name}'"),
                "navigate",
            ));
        }
    }

    // 1b. Helm named template `{{ include "name" . }}` / `{{ template "name" . }}` -> `define "name"`
    for tref in &parsed.helm_template_refs {
        for (tpath, trange) in index.find_helm_template_for(Some(&norm_path), &tref.value) {
            let tfname = tpath.rsplit('/').next().unwrap_or(&tpath);
            links.push((
                tref.range,
                Location {
                    path: tpath.clone(),
                    range: trange,
                },
                format!("Go to Helm template '{}' in {tfname}", tref.value),
                "navigate",
            ));
        }
    }

    // 1c. Reverse links from `values.yaml` keys -> `templates/*` usages in the same Helm chart
    if is_helm_values_filename(fname)
        && let Some(chart_root) = index.infer_helm_chart_dir(&norm_path)
    {
        for (val_key, val_sp) in &parsed.helm_values {
            for (tpath, tfile) in &index.files {
                if tpath == &norm_path {
                    continue;
                }
                let tdir = parent_dir(tpath);
                if tdir != chart_root && !tdir.starts_with(&format!("{chart_root}/")) {
                    continue;
                }
                for href in &tfile.helm_value_refs {
                    if &href.value == val_key {
                        let tfname = tpath.rsplit('/').next().unwrap_or(tpath);
                        links.push((
                            val_sp.range,
                            Location {
                                path: tpath.clone(),
                                range: href.range,
                            },
                            format!("Used in Helm template {tfname}"),
                            "implementing",
                        ));
                    }
                }
            }
        }
    }

    for kdoc in &parsed.docs {
        let kind = kdoc.kind.as_ref().map(|k| k.value.as_str()).unwrap_or("");

        // 1d. Custom Resource `kind` -> CustomResourceDefinition
        if let Some(ref kind_sp) = kdoc.kind
            && kind != "CustomResourceDefinition"
        {
            for (crd_path, _, crd_range) in index.find_crd(kind) {
                links.push((
                    kind_sp.range,
                    Location {
                        path: crd_path,
                        range: crd_range,
                    },
                    format!("Go to CustomResourceDefinition '{kind}'"),
                    "implemented",
                ));
            }
        }

        // 2. Kustomization file & directory paths -> target YAML / kustomization.yaml
        for kpath in kdoc
            .kustomize_paths
            .iter()
            .chain(kdoc.kustomize_helm_values.iter())
        {
            if let Some(resolved) = index.resolve_kustomize_target(&base_dir, &kpath.value)
                && let Some(target_file) = index.files.get(&resolved)
            {
                let target_range = target_file
                    .docs
                    .first()
                    .and_then(|d| d.name.as_ref().or(d.kind.as_ref()))
                    .map(|s| s.range)
                    .unwrap_or_default();
                links.push((
                    kpath.range,
                    Location {
                        path: resolved,
                        range: target_range,
                    },
                    format!("Go to Kustomize target {}", kpath.value),
                    "navigate",
                ));
            }
        }

        // 3. Service selector -> matching workloads, and targetPort -> containerPort
        if kind == "Service" {
            let workloads = index.find_matching_workloads(&kdoc.selector_labels);
            for sel_sp in kdoc.selector_labels.values() {
                for (wpath, wdoc, wrange) in &workloads {
                    let wkind = wdoc.kind.as_ref().map_or("Workload", |k| k.value.as_str());
                    let wname = wdoc.name.as_ref().map_or("unnamed", |n| n.value.as_str());
                    links.push((
                        sel_sp.range,
                        Location {
                            path: wpath.clone(),
                            range: *wrange,
                        },
                        format!("Go to matching {wkind} '{wname}'"),
                        "implemented",
                    ));
                }
            }
            for sport in &kdoc.service_ports {
                if let Some(ref tp) = sport.target_port {
                    for (wpath, wdoc, _) in &workloads {
                        for c in &wdoc.containers {
                            for cp in &c.ports {
                                if cp.value == tp.value {
                                    let wname =
                                        wdoc.name.as_ref().map_or("workload", |n| n.value.as_str());
                                    links.push((
                                        tp.range,
                                        Location {
                                            path: wpath.clone(),
                                            range: cp.range,
                                        },
                                        format!("Go to containerPort '{}' in {wname}", cp.value),
                                        "navigate",
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }

        // 4. Workload -> matching Services exposing it
        if kdoc.is_workload() {
            let services = index.find_matching_services(kdoc);
            if let Some(ref name_sp) = kdoc.name {
                for (spath, sdoc, srange) in services {
                    let sname = sdoc.name.as_ref().map_or("Service", |n| n.value.as_str());
                    links.push((
                        name_sp.range,
                        Location {
                            path: spath,
                            range: srange,
                        },
                        format!("Exposed by Service '{sname}'"),
                        "implementing",
                    ));
                }
            }
        }

        // 5. ConfigMap / Secret references (and specific key references)
        for dref in &kdoc.data_refs {
            for (tpath, tdoc, trange) in
                index.find_by_kind_and_name(dref.target_kind, &dref.name.value)
            {
                links.push((
                    dref.name.range,
                    Location {
                        path: tpath.clone(),
                        range: trange,
                    },
                    format!("Go to {} '{}'", dref.target_kind, dref.name.value),
                    "navigate",
                ));
                if let Some(ref key_sp) = dref.key
                    && let Some(target_key) = tdoc.data_keys.get(&key_sp.value)
                {
                    links.push((
                        key_sp.range,
                        Location {
                            path: tpath.clone(),
                            range: target_key.range,
                        },
                        format!(
                            "Go to key '{}' in {} '{}'",
                            key_sp.value, dref.target_kind, dref.name.value
                        ),
                        "navigate",
                    ));
                }
            }
        }

        // 6. ServiceAccount references
        for saref in &kdoc.service_account_refs {
            for (tpath, _, trange) in index.find_by_kind_and_name("ServiceAccount", &saref.value) {
                links.push((
                    saref.range,
                    Location {
                        path: tpath,
                        range: trange,
                    },
                    format!("Go to ServiceAccount '{}'", saref.value),
                    "navigate",
                ));
            }
        }

        // 7. PVC claimName references
        for pvcref in &kdoc.pvc_refs {
            for (tpath, _, trange) in
                index.find_by_kind_and_name("PersistentVolumeClaim", &pvcref.value)
            {
                links.push((
                    pvcref.range,
                    Location {
                        path: tpath,
                        range: trange,
                    },
                    format!("Go to PersistentVolumeClaim '{}'", pvcref.value),
                    "navigate",
                ));
            }
        }

        // 8. StorageClass references
        for scref in &kdoc.storage_class_refs {
            for (tpath, _, trange) in index.find_by_kind_and_name("StorageClass", &scref.value) {
                links.push((
                    scref.range,
                    Location {
                        path: tpath,
                        range: trange,
                    },
                    format!("Go to StorageClass '{}'", scref.value),
                    "navigate",
                ));
            }
        }

        // 9. RoleRef in RoleBinding / ClusterRoleBinding
        if let Some((ref rkind, ref rname)) = kdoc.role_ref {
            for (tpath, _, trange) in index.find_by_kind_and_name(rkind, &rname.value) {
                links.push((
                    rname.range,
                    Location {
                        path: tpath,
                        range: trange,
                    },
                    format!("Go to {rkind} '{}'", rname.value),
                    "navigate",
                ));
            }
        }

        // 10. Ingress backend Service & Port references
        for backend in &kdoc.ingress_backends {
            for (spath, sdoc, srange) in
                index.find_by_kind_and_name("Service", &backend.service_name.value)
            {
                links.push((
                    backend.service_name.range,
                    Location {
                        path: spath.clone(),
                        range: srange,
                    },
                    format!("Go to Service '{}'", backend.service_name.value),
                    "navigate",
                ));
                if let Some(ref pname) = backend.port_name {
                    for sp in &sdoc.service_ports {
                        if let Some(ref sn) = sp.name
                            && sn.value == pname.value
                        {
                            links.push((
                                pname.range,
                                Location {
                                    path: spath.clone(),
                                    range: sn.range,
                                },
                                format!(
                                    "Go to port '{}' on Service '{}'",
                                    pname.value, backend.service_name.value
                                ),
                                "navigate",
                            ));
                        }
                    }
                }
                if let Some(ref pnum) = backend.port_number {
                    for sp in &sdoc.service_ports {
                        if let Some(ref sn) = sp.port
                            && sn.value == pnum.value
                        {
                            links.push((
                                pnum.range,
                                Location {
                                    path: spath.clone(),
                                    range: sn.range,
                                },
                                format!(
                                    "Go to port {} on Service '{}'",
                                    pnum.value, backend.service_name.value
                                ),
                                "navigate",
                            ));
                        }
                    }
                }
            }
        }

        // 11. HPA / Autoscaler `scaleTargetRef.name` -> target workload
        if let Some((ref target_kind, ref target_name)) = kdoc.scale_target_ref {
            let single_kind;
            let candidate_kinds: &[&str] = match target_kind.as_deref() {
                Some(k) if !k.is_empty() => {
                    single_kind = [k];
                    &single_kind
                }
                _ => &["Deployment", "StatefulSet", "ReplicaSet"],
            };
            for &ck in candidate_kinds {
                for (tpath, _, trange) in index.find_by_kind_and_name(ck, &target_name.value) {
                    links.push((
                        target_name.range,
                        Location {
                            path: tpath,
                            range: trange,
                        },
                        format!("Go to {ck} '{}'", target_name.value),
                        "navigate",
                    ));
                }
            }
        }

        // 12. Container `volumeMounts[].name` -> Pod `volumes[].name` in the same document
        for vm in &kdoc.volume_mounts {
            for vol in &kdoc.volumes {
                if vol.value == vm.value {
                    links.push((
                        vm.range,
                        Location {
                            path: norm_path.clone(),
                            range: vol.range,
                        },
                        format!("Go to volume '{}'", vol.value),
                        "navigate",
                    ));
                }
            }
        }
    }

    links
}

/// Provides `definition` locations at `at`.
pub fn definitions_at(
    index: &ProjectIndex,
    doc: &Document,
    at: Position,
    parsed: &ParsedFile,
) -> Vec<Location> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for (range, loc, _, _) in document_links(index, &doc.path, parsed) {
        if contains(range, at)
            && seen.insert((
                loc.path.clone(),
                loc.range.start.line,
                loc.range.start.character,
            ))
        {
            out.push(loc);
        }
    }
    out
}

/// Provides gutter `LineMarker` items for `doc`.
pub fn line_markers_for(
    index: &ProjectIndex,
    doc: &Document,
    parsed: &ParsedFile,
) -> Vec<LineMarker> {
    let mut by_line: BTreeMap<u32, LineMarker> = BTreeMap::new();
    for (range, loc, tooltip, icon) in document_links(index, &doc.path, parsed) {
        by_line
            .entry(range.start.line)
            .or_insert_with(|| LineMarker {
                line: range.start.line,
                icon: Some(icon.to_owned()),
                tooltip,
                target: Some(loc),
            });
    }
    by_line.into_values().collect()
}

pub fn diag_with_source(
    range: Range,
    severity: Severity,
    message: impl Into<String>,
    code: &str,
    source: &str,
    deprecated: bool,
    unnecessary: bool,
) -> Diagnostic {
    let mut d = Diagnostic::new(range, severity, message).code(code);
    d.source = Some(source.to_owned());
    d.deprecated = deprecated;
    d.unnecessary = unnecessary;
    d
}

pub fn quickfix(title: impl Into<String>, edits: Vec<(String, Vec<TextEdit>)>) -> CodeAction {
    CodeAction {
        title: title.into(),
        kind: Some("quickfix".into()),
        preferred: true,
        edits,
        data: None,
    }
}

pub fn snippet_item(
    label: &str,
    insert_text: impl Into<String>,
    replace: Range,
    detail: &str,
) -> CompletionItem {
    CompletionItem {
        label: label.to_owned(),
        detail: Some(detail.to_owned()),
        kind: Some(CompletionKind::Snippet),
        insert_text: insert_text.into(),
        snippet: true,
        replace: Some(replace),
        sort_text: None,
        filter_text: None,
        documentation: None,
        additional_edits: Vec::new(),
    }
}

/// Runs static Kubernetes manifest inspections on `parsed` (for `doc_path` / `text`),
/// returning both diagnostics and quick-fix `CodeAction`s.
pub fn inspect_document(
    index: &ProjectIndex,
    doc_path: &str,
    text: &str,
    parsed: &ParsedFile,
    warn_latest_tag: bool,
    warn_missing_resources: bool,
) -> Vec<InspectionIssue> {
    let mut issues = Vec::new();
    if parsed.marked_non_k8s {
        return issues;
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let norm_path = normalize_slashes(doc_path);
    let base_dir = parent_dir(&norm_path);
    let has_multiple_manifest_files = index.files.len() > 1;
    let is_kustomize_patch = index.is_kustomize_patch_path(&norm_path);

    for kdoc in &parsed.docs {
        if !kdoc.is_kube_resource() {
            continue;
        }
        let kind = kdoc.kind.as_ref().map(|k| k.value.as_str()).unwrap_or("");
        let api = kdoc
            .api_version
            .as_ref()
            .map(|a| a.value.as_str())
            .unwrap_or("");

        // 1. Deprecated apiVersion check + Quick Fix
        if let Some(ref api_sp) = kdoc.api_version {
            for &(old_api, target_kind, replacement_api) in DEPRECATED_APIS {
                if api == old_api && (kind == target_kind || kind.is_empty()) {
                    let message = format!(
                        "Deprecated apiVersion '{old_api}' for {target_kind}; use '{replacement_api}' instead"
                    );
                    let diag = diag_with_source(
                        api_sp.range,
                        Severity::Warning,
                        &message,
                        "deprecated-api-version",
                        "kubernetes",
                        true,
                        false,
                    );
                    let fix = quickfix(
                        format!("Kubernetes: Upgrade apiVersion to {replacement_api}"),
                        vec![(
                            doc_path.to_owned(),
                            vec![TextEdit {
                                range: api_sp.range,
                                text: replacement_api.to_owned(),
                            }],
                        )],
                    );
                    issues.push(InspectionIssue {
                        diagnostic: diag,
                        fix: Some(fix),
                    });
                }
            }
        }

        // 2. Missing `metadata.name` on standard Kubernetes objects (excluding Kustomization & Kustomize patches)
        if !is_kustomize_patch
            && !kind.is_empty()
            && kind != "Kustomization"
            && kind != "Component"
            && kind != "List"
            && kdoc.name.is_none()
            && let Some(ref kind_sp) = kdoc.kind
        {
            let diag = diag_with_source(
                kind_sp.range,
                Severity::Error,
                format!("{kind} resource is missing 'metadata.name'"),
                "missing-metadata-name",
                "kubernetes",
                false,
                false,
            );
            let insert_line = kind_sp.line + 1;
            let edit_pos = Position::new(insert_line, 0);
            let default_name = format!("my-{}\n", kind.to_ascii_lowercase());
            let snippet = if text.contains("metadata:") {
                format!("  name: {default_name}")
            } else {
                format!("metadata:\n  name: {default_name}")
            };
            let fix = quickfix(
                "Kubernetes: Add metadata.name",
                vec![(
                    doc_path.to_owned(),
                    vec![TextEdit {
                        range: Range::new(edit_pos, edit_pos),
                        text: snippet,
                    }],
                )],
            );
            issues.push(InspectionIssue {
                diagnostic: diag,
                fix: Some(fix),
            });
        }

        // 3. Workload selector.matchLabels vs spec.template.metadata.labels mismatch
        if !is_kustomize_patch
            && matches!(
                kind,
                "Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet"
            )
            && !kdoc.selector_labels.is_empty()
        {
            for (sel_k, sel_v) in &kdoc.selector_labels {
                let matches_pod = kdoc
                    .pod_labels
                    .get(sel_k)
                    .is_some_and(|pv| pv.value == sel_v.value);
                if !matches_pod {
                    let message = format!(
                        "Selector label '{sel_k}: {}' does not match 'spec.template.metadata.labels'",
                        sel_v.value
                    );
                    let diag = diag_with_source(
                        sel_v.range,
                        Severity::Error,
                        &message,
                        "selector-label-mismatch",
                        "kubernetes",
                        false,
                        false,
                    );

                    let fix = if let Some(existing_pod_label) = kdoc.pod_labels.get(sel_k) {
                        Some(quickfix(
                            format!(
                                "Kubernetes: Set template label '{sel_k}' to '{}'",
                                sel_v.value
                            ),
                            vec![(
                                doc_path.to_owned(),
                                vec![TextEdit {
                                    range: existing_pod_label.range,
                                    text: sel_v.value.clone(),
                                }],
                            )],
                        ))
                    } else if let (Some(labels_line), Some(labels_indent)) =
                        (kdoc.pod_labels_line, kdoc.pod_labels_indent)
                    {
                        let pos = Position::new(labels_line + 1, 0);
                        let pad = " ".repeat(labels_indent + 2);
                        Some(quickfix(
                            format!(
                                "Kubernetes: Add '{sel_k}: {}' to pod template labels",
                                sel_v.value
                            ),
                            vec![(
                                doc_path.to_owned(),
                                vec![TextEdit {
                                    range: Range::new(pos, pos),
                                    text: format!("{pad}{sel_k}: {}\n", sel_v.value),
                                }],
                            )],
                        ))
                    } else {
                        None
                    };
                    issues.push(InspectionIssue {
                        diagnostic: diag,
                        fix,
                    });
                }
            }
        }

        // 4. Container inspections: `:latest` / untagged image, missing resources, privileged container
        for container in &kdoc.containers {
            let cname = container
                .name
                .as_ref()
                .map_or("container", |n| n.value.as_str());
            if warn_latest_tag
                && let Some(ref img) = container.image
                && !img.value.contains("{{")
            {
                let last_segment = img.value.rsplit('/').next().unwrap_or(&img.value);
                let is_latest_or_untagged = img.value.ends_with(":latest")
                    || (!last_segment.contains(':') && !img.value.contains('@'));
                if is_latest_or_untagged {
                    let msg = format!(
                        "Container '{cname}' image '{}' uses ':latest' or omits an explicit version tag",
                        img.value
                    );
                    let diag = diag_with_source(
                        img.range,
                        Severity::Warning,
                        &msg,
                        "image-latest-tag",
                        "kubernetes",
                        false,
                        false,
                    );
                    let base_img = img.value.strip_suffix(":latest").unwrap_or(&img.value);
                    let pinned = format!("{base_img}:1.0.0");
                    let fix = quickfix(
                        format!("Kubernetes: Pin image version tag ({pinned})"),
                        vec![(
                            doc_path.to_owned(),
                            vec![TextEdit {
                                range: img.range,
                                text: pinned,
                            }],
                        )],
                    );
                    issues.push(InspectionIssue {
                        diagnostic: diag,
                        fix: Some(fix),
                    });
                }
            }

            if warn_missing_resources
                && !is_kustomize_patch
                && kdoc.is_workload()
                && (!container.has_limits || !container.has_requests)
            {
                let anchor_range = container
                    .name
                    .as_ref()
                    .or(container.image.as_ref())
                    .map(|s| s.range)
                    .unwrap_or_else(|| {
                        let line_len = lines
                            .get(container.header_line as usize)
                            .map_or(0, |l| utf16_len(l));
                        Range::new(
                            Position::new(container.header_line, 0),
                            Position::new(container.header_line, line_len),
                        )
                    });
                let diag = diag_with_source(
                    anchor_range,
                    Severity::Warning,
                    format!(
                        "Container '{cname}' is missing CPU/memory resource requests and limits"
                    ),
                    "missing-resource-limits",
                    "kubernetes",
                    false,
                    false,
                );

                if !container.has_resources {
                    let insert_line = container.end_line + 1;
                    let pad = " ".repeat(container.indent + 2);
                    let block = format!(
                        "{pad}resources:\n{pad}  requests:\n{pad}    cpu: 100m\n{pad}    memory: 128Mi\n{pad}  limits:\n{pad}    cpu: 500m\n{pad}    memory: 256Mi\n"
                    );
                    let pos = if (insert_line as usize) <= lines.len() {
                        Position::new(insert_line, 0)
                    } else {
                        Position::new(container.end_line, lines.last().map_or(0, |l| utf16_len(l)))
                    };
                    let prefix = if (insert_line as usize) > lines.len() && !text.ends_with('\n') {
                        "\n"
                    } else {
                        ""
                    };
                    let fix = quickfix(
                        format!("Kubernetes: Add resource requests and limits to '{cname}'"),
                        vec![(
                            doc_path.to_owned(),
                            vec![TextEdit {
                                range: Range::new(pos, pos),
                                text: format!("{prefix}{block}"),
                            }],
                        )],
                    );
                    issues.push(InspectionIssue {
                        diagnostic: diag,
                        fix: Some(fix),
                    });
                } else {
                    issues.push(InspectionIssue {
                        diagnostic: diag,
                        fix: None,
                    });
                }
            }

            if let Some(ref priv_sp) = container.privileged
                && priv_sp.value == "true"
            {
                let diag = diag_with_source(
                    priv_sp.range,
                    Severity::Warning,
                    format!(
                        "Container '{cname}' runs in privileged mode (securityContext.privileged: true)"
                    ),
                    "privileged-container",
                    "kubernetes",
                    false,
                    false,
                );
                let fix = quickfix(
                    "Kubernetes: Set privileged to false",
                    vec![(
                        doc_path.to_owned(),
                        vec![TextEdit {
                            range: priv_sp.range,
                            text: "false".into(),
                        }],
                    )],
                );
                issues.push(InspectionIssue {
                    diagnostic: diag,
                    fix: Some(fix),
                });
            }
        }

        // 5. Missing ConfigMap / Secret key reference in same project
        for dref in &kdoc.data_refs {
            if dref.name.value.contains("{{") {
                continue;
            }
            let targets = index.find_by_kind_and_name(dref.target_kind, &dref.name.value);
            if let Some(ref key_sp) = dref.key
                && !key_sp.value.contains("{{")
            {
                for (tpath, tdoc, _) in &targets {
                    if !tdoc.data_keys.contains_key(&key_sp.value) {
                        let diag = diag_with_source(
                            key_sp.range,
                            Severity::Warning,
                            format!(
                                "Key '{}' is not defined in {} '{}'",
                                key_sp.value, dref.target_kind, dref.name.value
                            ),
                            "unresolved-config-key",
                            "kubernetes",
                            false,
                            false,
                        );

                        let fix = if let (Some(data_indent), Some(data_end)) =
                            (tdoc.data_indent, tdoc.data_end_line)
                        {
                            let pad = " ".repeat(data_indent + 2);
                            let pos = Position::new(data_end + 1, 0);
                            Some(quickfix(
                                format!(
                                    "Kubernetes: Add key '{}' to {} '{}'",
                                    key_sp.value, dref.target_kind, dref.name.value
                                ),
                                vec![(
                                    tpath.clone(),
                                    vec![TextEdit {
                                        range: Range::new(pos, pos),
                                        text: format!("{pad}{}: \"\"\n", key_sp.value),
                                    }],
                                )],
                            ))
                        } else {
                            None
                        };
                        issues.push(InspectionIssue {
                            diagnostic: diag,
                            fix,
                        });
                    }
                }
            } else if targets.is_empty() && (parsed.docs.len() > 1 || has_multiple_manifest_files) {
                let diag = diag_with_source(
                    dref.name.range,
                    Severity::Info,
                    format!(
                        "Referenced {} '{}' was not found in workspace manifests",
                        dref.target_kind, dref.name.value
                    ),
                    "unresolved-resource-ref",
                    "kubernetes",
                    false,
                    false,
                );
                issues.push(InspectionIssue {
                    diagnostic: diag,
                    fix: None,
                });
            }
        }

        // 6. Duplicate YAML keys within the same mapping
        for (dup_key, dup_range, first_line) in &kdoc.duplicate_keys {
            let diag = diag_with_source(
                *dup_range,
                Severity::Error,
                format!(
                    "Duplicate YAML key '{dup_key}' (first defined on line {})",
                    first_line + 1
                ),
                "duplicate-yaml-key",
                "kubernetes",
                false,
                false,
            );
            let remove_range = Range::new(
                Position::new(dup_range.start.line, 0),
                Position::new(dup_range.start.line + 1, 0),
            );
            let fix = quickfix(
                format!("Kubernetes: Remove duplicate key '{dup_key}'"),
                vec![(
                    doc_path.to_owned(),
                    vec![TextEdit {
                        range: remove_range,
                        text: String::new(),
                    }],
                )],
            );
            issues.push(InspectionIssue {
                diagnostic: diag,
                fix: Some(fix),
            });
        }

        // 7. Read-only / server-managed fields (`metadata.uid`, `metadata.resourceVersion`, `status`, etc.)
        for (ro_path, ro_range) in &kdoc.readonly_fields {
            let diag = diag_with_source(
                *ro_range,
                Severity::Info,
                format!("'{ro_path}' is a server-managed read-only field in Kubernetes manifests"),
                "readonly-field",
                "kubernetes",
                false,
                true,
            );
            let remove_range = Range::new(
                Position::new(ro_range.start.line, 0),
                Position::new(ro_range.start.line + 1, 0),
            );
            let fix = quickfix(
                format!("Kubernetes: Remove read-only field '{ro_path}'"),
                vec![(
                    doc_path.to_owned(),
                    vec![TextEdit {
                        range: remove_range,
                        text: String::new(),
                    }],
                )],
            );
            issues.push(InspectionIssue {
                diagnostic: diag,
                fix: Some(fix),
            });
        }

        // 8. Deprecated fields (`bases`, `patchesStrategicMerge`, `commonLabels`, `spec.serviceAccount`)
        for (dep_field, replacement, dep_range) in &kdoc.deprecated_fields {
            let diag = diag_with_source(
                *dep_range,
                Severity::Warning,
                format!("Deprecated field '{dep_field}'; use '{replacement}' instead"),
                "deprecated-field",
                "kubernetes",
                true,
                false,
            );
            let fix = quickfix(
                format!("Kubernetes: Replace '{dep_field}' with '{replacement}'"),
                vec![(
                    doc_path.to_owned(),
                    vec![TextEdit {
                        range: *dep_range,
                        text: replacement.clone(),
                    }],
                )],
            );
            issues.push(InspectionIssue {
                diagnostic: diag,
                fix: Some(fix),
            });
        }

        // 9. Invalid enum values (`spec.type`, `imagePullPolicy`, `restartPolicy`, `pathType`)
        for (enum_path, actual, allowed, enum_range) in &kdoc.invalid_enums {
            let diag = diag_with_source(
                *enum_range,
                Severity::Error,
                format!(
                    "Invalid value '{actual}' for '{enum_path}' (expected one of: {})",
                    allowed.join(", ")
                ),
                "invalid-enum-value",
                "kubernetes",
                false,
                false,
            );
            let default_val = allowed[0];
            let fix = quickfix(
                format!("Kubernetes: Change '{enum_path}' to '{default_val}'"),
                vec![(
                    doc_path.to_owned(),
                    vec![TextEdit {
                        range: *enum_range,
                        text: default_val.to_owned(),
                    }],
                )],
            );
            issues.push(InspectionIssue {
                diagnostic: diag,
                fix: Some(fix),
            });
        }

        // 10. Kustomization missing local file path inspection
        if has_multiple_manifest_files {
            for kpath in &kdoc.kustomize_paths {
                if kpath.value.starts_with("http://")
                    || kpath.value.starts_with("https://")
                    || kpath.value.starts_with("ssh://")
                {
                    continue;
                }
                let resolved = join_relative(&base_dir, &kpath.value);
                let exists = index.files.contains_key(&resolved)
                    || index
                        .files
                        .keys()
                        .any(|k| k.starts_with(&format!("{resolved}/")));
                if !exists {
                    let diag = diag_with_source(
                        kpath.range,
                        Severity::Warning,
                        format!(
                            "Kustomize path '{}' does not match any manifest in the project",
                            kpath.value
                        ),
                        "unresolved-kustomize-path",
                        "kubernetes",
                        false,
                        false,
                    );
                    issues.push(InspectionIssue {
                        diagnostic: diag,
                        fix: None,
                    });
                }
            }
        }
    }

    // If any issues were reported, offer a file-level quick fix to mark the file as `# nonk8s` (IntelliJ parity).
    if !issues.is_empty()
        && let Some(first) = issues.first()
    {
        let start = Position::new(0, 0);
        let mark_non_k8s = CodeAction {
            title: "Kubernetes: Mark file as non-Kubernetes (# nonk8s)".into(),
            kind: Some("quickfix".into()),
            preferred: false,
            edits: vec![(
                doc_path.to_owned(),
                vec![TextEdit {
                    range: Range::new(start, start),
                    text: "# nonk8s\n".into(),
                }],
            )],
            data: None,
        };
        issues.push(InspectionIssue {
            diagnostic: first.diagnostic.clone(),
            fix: Some(mark_non_k8s),
        });
    }

    issues
}

/// Computes relative path from `from_dir` to `target_path` using `/` separators.
fn relative_path_between(from_dir: &str, target_path: &str) -> Option<String> {
    let from_norm = normalize_slashes(from_dir);
    let to_norm = normalize_slashes(target_path);
    if from_norm.starts_with('/') != to_norm.starts_with('/') {
        return None;
    }
    let from_segs: Vec<&str> = from_norm.split('/').filter(|s| !s.is_empty()).collect();
    let to_segs: Vec<&str> = to_norm.split('/').filter(|s| !s.is_empty()).collect();
    let common = from_segs
        .iter()
        .zip(to_segs.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let ups = from_segs.len().saturating_sub(common);
    if ups > 4 {
        return None;
    }
    let mut out: Vec<&str> = std::iter::repeat_n("..", ups).collect();
    out.extend_from_slice(&to_segs[common..]);
    if out.is_empty() {
        Some(".".into())
    } else {
        Some(out.join("/"))
    }
}

const HELM_RELEASE_FIELDS: &[(&str, &str)] = &[
    ("Name", "Release name"),
    ("Namespace", "Target release namespace"),
    ("Service", "Release service (Helm)"),
    (
        "IsUpgrade",
        "True when current operation is an upgrade or rollback",
    ),
    ("IsInstall", "True when current operation is an install"),
    ("Revision", "Release revision number"),
];

const HELM_CHART_FIELDS: &[(&str, &str)] = &[
    ("Name", "Chart name from Chart.yaml"),
    ("Version", "Chart version from Chart.yaml"),
    ("AppVersion", "Application version from Chart.yaml"),
    ("Description", "Chart description"),
    ("Type", "Chart type (application or library)"),
    ("Keywords", "Chart keywords list"),
    ("Home", "Chart home URL"),
    ("KubeVersion", "Required Kubernetes version constraint"),
];

const HELM_CAPABILITIES_FIELDS: &[(&str, &str)] = &[
    ("KubeVersion", "Target cluster Kubernetes version info"),
    (
        "APIVersions",
        "Set of available API versions on the cluster",
    ),
    ("HelmVersion", "Helm client build info"),
];

const HELM_FILES_FIELDS: &[(&str, &str)] = &[
    ("Get", "Read file contents as string"),
    ("GetBytes", "Read file contents as byte slice"),
    ("Glob", "Match chart files by glob pattern"),
    ("Lines", "Iterate over lines of a file"),
    ("AsSecrets", "Base64-encode matched files for Secret data"),
    (
        "AsConfig",
        "Format matched files as YAML map for ConfigMap data",
    ),
];

const HELM_TEMPLATE_OBJ_FIELDS: &[(&str, &str)] = &[
    (
        "Name",
        "Current template path (e.g. mychart/templates/deployment.yaml)",
    ),
    ("BasePath", "Current chart templates directory path"),
];

const HELM_KEYWORDS: &[(&str, &str)] = &[
    ("if", "Conditional block"),
    ("else", "Fallback branch"),
    ("end", "Close template block"),
    ("range", "Iterate over list or map"),
    ("with", "Rebind dot (.) scope if non-empty"),
    ("define", "Declare named template"),
    ("include", "Render named template as string (pipeable)"),
    ("template", "Render named template inline"),
    ("block", "Define overridable template block"),
    ("not", "Boolean negation"),
    ("and", "Short-circuit boolean AND"),
    ("or", "Short-circuit boolean OR"),
    ("eq", "Equal comparison"),
    ("ne", "Not-equal comparison"),
    ("lt", "Less-than comparison"),
    ("le", "Less-than-or-equal comparison"),
    ("gt", "Greater-than comparison"),
    ("ge", "Greater-than-or-equal comparison"),
];

const HELM_FUNCTIONS: &[(&str, &str)] = &[
    ("default", "Fallback value when empty"),
    ("required", "Fail rendering when value is empty"),
    ("quote", "Wrap value in double quotes"),
    ("squote", "Wrap value in single quotes"),
    ("toYaml", "Serialize value to YAML"),
    ("fromYaml", "Parse YAML string into map"),
    ("toJson", "Serialize value to JSON"),
    ("fromJson", "Parse JSON string into map"),
    ("nindent", "Prepend newline and indent every line"),
    ("indent", "Indent every line by N spaces"),
    ("printf", "Format string with arguments"),
    ("print", "Concatenate values as string"),
    ("trunc", "Truncate string to N characters"),
    ("trim", "Strip leading and trailing whitespace"),
    ("trimSuffix", "Strip suffix if present"),
    ("trimPrefix", "Strip prefix if present"),
    ("replace", "Replace substrings"),
    ("lower", "Convert string to lowercase"),
    ("upper", "Convert string to uppercase"),
    ("title", "Convert string to title case"),
    ("contains", "Test whether string contains substring"),
    ("hasPrefix", "Test whether string starts with prefix"),
    ("hasSuffix", "Test whether string ends with suffix"),
    ("b64enc", "Base64-encode string"),
    ("b64dec", "Base64-decode string"),
    ("sha256sum", "Compute SHA-256 hex digest"),
    ("dict", "Construct key-value dictionary"),
    ("list", "Construct slice of values"),
    ("merge", "Deep-merge maps (destination wins)"),
    ("hasKey", "Test whether dictionary contains key"),
    ("pluck", "Extract field across a list of maps"),
    ("coalesce", "Return first non-empty value"),
    ("ternary", "Select first or second value by boolean"),
    ("tpl", "Evaluate string as Helm template"),
    ("lookup", "Query live cluster resource during render"),
];

/// Appends chart-scoped `values.yaml` completions under `base_prefix` ("" for root `.Values.`)
/// matching `typed_rel` (which may contain dots like `image.t` or be empty).
fn push_helm_values_completions(
    index: &ProjectIndex,
    doc_path: &str,
    at: Position,
    base_prefix: &str,
    typed_rel: &str,
    include_full_paths_at_root: bool,
    items: &mut Vec<CompletionItem>,
) {
    let (sub_prefix, seg_typed) = match typed_rel.rfind('.') {
        Some(dot) => (&typed_rel[..=dot], &typed_rel[dot + 1..]),
        None => ("", typed_rel),
    };
    let effective_prefix = match (base_prefix.is_empty(), sub_prefix.is_empty()) {
        (true, true) => String::new(),
        (true, false) => sub_prefix.to_owned(),
        (false, true) => format!("{base_prefix}."),
        (false, false) => format!("{base_prefix}.{sub_prefix}"),
    };
    let start_col = at.character.saturating_sub(utf16_len(seg_typed));
    let replace = Range::new(Position::new(at.line, start_col), at);

    let chart_root = index.infer_helm_chart_dir(doc_path);
    let mut candidate_files: Vec<(&str, &ParsedFile)> = Vec::new();
    let mut fallback_files: Vec<(&str, &ParsedFile)> = Vec::new();
    for (path, file) in &index.files {
        let fname = path.rsplit('/').next().unwrap_or(path);
        if is_helm_values_filename(fname) {
            if chart_root.as_ref().is_some_and(|root| {
                let dir = parent_dir(path);
                dir == *root || dir.starts_with(&format!("{root}/"))
            }) {
                candidate_files.push((fname, file));
            } else {
                fallback_files.push((fname, file));
            }
        }
    }
    if candidate_files.is_empty() {
        candidate_files = fallback_files;
    }

    let mut seen = BTreeSet::new();
    for (fname, file) in candidate_files {
        for (k, sp) in &file.helm_values {
            let Some(rem) = (if effective_prefix.is_empty() {
                Some(k.as_str())
            } else {
                k.strip_prefix(&effective_prefix)
            }) else {
                continue;
            };
            if rem.is_empty() || !rem.starts_with(seg_typed) {
                continue;
            }
            let first_seg = rem.split('.').next().unwrap_or(rem);
            if seen.insert(first_seg.to_owned()) {
                let detail = if first_seg == rem && !sp.value.is_empty() {
                    format!("{} ({fname})", sp.value)
                } else {
                    fname.to_owned()
                };
                items.push(CompletionItem {
                    replace: Some(replace),
                    detail: Some(detail),
                    ..CompletionItem::new(first_seg, CompletionKind::Property)
                });
            }
            if include_full_paths_at_root
                && sub_prefix.is_empty()
                && rem != first_seg
                && seen.insert(rem.to_owned())
            {
                let detail = if sp.value.is_empty() {
                    fname.to_owned()
                } else {
                    format!("{} ({fname})", sp.value)
                };
                items.push(CompletionItem {
                    replace: Some(replace),
                    detail: Some(detail),
                    ..CompletionItem::new(rem, CompletionKind::Property)
                });
            }
        }
    }
}

/// Computes completions when cursor `at` sits inside an open `{{ ... }}` Helm action.
fn helm_action_completions(
    index: &ProjectIndex,
    doc_path: &str,
    at: Position,
    ctx: &HelmActionContext,
) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    let prefix = &ctx.action_prefix;

    // 1. Named template completion: `include "xxx` or `template "xxx`
    if let Some(idx) = prefix
        .rfind("include \"")
        .or_else(|| prefix.rfind("template \""))
    {
        let quote_pos = prefix[idx..].find('"').unwrap_or(0) + idx + 1;
        let typed = &prefix[quote_pos..];
        if !typed.contains('"') {
            let start_col = at.character.saturating_sub(utf16_len(typed));
            let replace = Range::new(Position::new(at.line, start_col), at);
            let chart_root = index.infer_helm_chart_dir(doc_path);
            let mut local = Vec::new();
            let mut fallback = Vec::new();
            for (path, file) in &index.files {
                let fname = path.rsplit('/').next().unwrap_or(path);
                let in_chart = chart_root.as_ref().is_some_and(|root| {
                    let dir = parent_dir(path);
                    dir == *root || dir.starts_with(&format!("{root}/"))
                });
                for name in file.helm_template_defs.keys() {
                    if name.starts_with(typed) {
                        let entry = (name.clone(), fname.to_owned());
                        if in_chart {
                            local.push(entry);
                        } else {
                            fallback.push(entry);
                        }
                    }
                }
            }
            let chosen = if !local.is_empty() { local } else { fallback };
            let mut seen = BTreeSet::new();
            for (name, fname) in chosen {
                if seen.insert(name.clone()) {
                    items.push(CompletionItem {
                        replace: Some(replace),
                        detail: Some(format!("Helm template in {fname}")),
                        ..CompletionItem::new(&name, CompletionKind::Function)
                    });
                }
            }
            return items;
        }
    }

    // Extract the active expression token right before cursor (bounded by whitespace, `(`, `|`, `=`)
    let tok_start = prefix
        .rfind(|c: char| c.is_ascii_whitespace() || matches!(c, '(' | '|' | '=' | ':'))
        .map_or(0, |i| i + 1);
    let token = &prefix[tok_start..];
    let before_tok = prefix[..tok_start].trim_end();
    let after_pipe = before_tok.ends_with('|');

    // 2. `$.Values.<typed>` or `.Values.<typed>`
    if let Some(typed_rel) = token
        .strip_prefix("$.Values.")
        .or_else(|| token.strip_prefix(".Values."))
    {
        push_helm_values_completions(index, doc_path, at, "", typed_rel, true, &mut items);
        return items;
    }

    // 3. Built-in Helm object fields (`.Release.*`, `.Chart.*`, `.Capabilities.*`, `.Files.*`, `.Template.*`)
    for (obj_prefix, table) in [
        (".Release.", HELM_RELEASE_FIELDS),
        ("$.Release.", HELM_RELEASE_FIELDS),
        (".Chart.", HELM_CHART_FIELDS),
        ("$.Chart.", HELM_CHART_FIELDS),
        (".Capabilities.", HELM_CAPABILITIES_FIELDS),
        ("$.Capabilities.", HELM_CAPABILITIES_FIELDS),
        (".Files.", HELM_FILES_FIELDS),
        ("$.Files.", HELM_FILES_FIELDS),
        (".Template.", HELM_TEMPLATE_OBJ_FIELDS),
        ("$.Template.", HELM_TEMPLATE_OBJ_FIELDS),
    ] {
        if let Some(typed) = token.strip_prefix(obj_prefix)
            && !typed.contains('.')
        {
            let start_col = at.character.saturating_sub(utf16_len(typed));
            let replace = Range::new(Position::new(at.line, start_col), at);
            for &(field, desc) in table {
                if field.starts_with(typed) {
                    items.push(CompletionItem {
                        replace: Some(replace),
                        detail: Some(desc.into()),
                        ..CompletionItem::new(field, CompletionKind::Property)
                    });
                }
            }
            return items;
        }
    }

    // 4. `$var` or `$var.<subpath>` completions
    if let Some(after_dollar) = token.strip_prefix('$') {
        if let Some(dot_idx) = after_dollar.find('.') {
            let var_name = &token[..dot_idx + 1];
            let sub_typed = &after_dollar[dot_idx + 1..];
            if var_name == "$" {
                // `$.<root>` -> root objects
                if !sub_typed.contains('.') {
                    let start_col = at.character.saturating_sub(utf16_len(sub_typed));
                    let replace = Range::new(Position::new(at.line, start_col), at);
                    for &(root_obj, desc) in &[
                        ("Values", "Chart values from values.yaml"),
                        ("Release", "Helm release metadata"),
                        ("Chart", "Chart.yaml metadata"),
                        ("Capabilities", "Cluster capabilities"),
                        ("Files", "Chart file access helpers"),
                        ("Template", "Current template metadata"),
                    ] {
                        if root_obj.starts_with(sub_typed) {
                            items.push(CompletionItem {
                                replace: Some(replace),
                                detail: Some(desc.into()),
                                ..CompletionItem::new(root_obj, CompletionKind::Module)
                            });
                        }
                    }
                }
                return items;
            }
            if let Some(Some(bound_base)) = ctx.vars.get(var_name) {
                push_helm_values_completions(
                    index, doc_path, at, bound_base, sub_typed, false, &mut items,
                );
            }
            return items;
        }
        // Complete `$var` names in scope
        let start_col = at.character.saturating_sub(utf16_len(token));
        let replace = Range::new(Position::new(at.line, start_col), at);
        for (vname, bound) in &ctx.vars {
            if vname.starts_with(token) {
                let detail = match bound {
                    Some(b) if !b.is_empty() => format!("Helm variable (.Values.{b})"),
                    _ => "Helm template variable".into(),
                };
                items.push(CompletionItem {
                    replace: Some(replace),
                    detail: Some(detail),
                    ..CompletionItem::new(vname, CompletionKind::Variable)
                });
            }
        }
        return items;
    }

    // 5. Leading `.` (`{{ .` or `{{ .na` or `{{ .destination.na`)
    if let Some(after_dot) = token.strip_prefix('.') {
        if !after_dot.contains('.') {
            let start_col = at.character.saturating_sub(utf16_len(after_dot));
            let replace = Range::new(Position::new(at.line, start_col), at);
            for &(root_obj, desc) in &[
                ("Values", "Chart values from values.yaml"),
                ("Release", "Helm release metadata"),
                ("Chart", "Chart.yaml metadata"),
                ("Capabilities", "Cluster capabilities"),
                ("Files", "Chart file access helpers"),
                ("Template", "Current template metadata"),
            ] {
                if root_obj.starts_with(after_dot) {
                    items.push(CompletionItem {
                        replace: Some(replace),
                        detail: Some(desc.into()),
                        ..CompletionItem::new(root_obj, CompletionKind::Module)
                    });
                }
            }
        }
        if let Some(ref scope_base) = ctx.dot_scope
            && !scope_base.is_empty()
        {
            push_helm_values_completions(
                index, doc_path, at, scope_base, after_dot, false, &mut items,
            );
        }
        return items;
    }

    // 6. Pipeline functions (`| quote`) or keywords + functions inside `{{ ... }}`
    let start_col = at.character.saturating_sub(utf16_len(token));
    let replace = Range::new(Position::new(at.line, start_col), at);
    if !after_pipe {
        for &(kw, desc) in HELM_KEYWORDS {
            if kw.starts_with(token) {
                items.push(CompletionItem {
                    replace: Some(replace),
                    detail: Some(desc.into()),
                    ..CompletionItem::new(kw, CompletionKind::Keyword)
                });
            }
        }
    }
    for &(fn_name, desc) in HELM_FUNCTIONS {
        if fn_name.starts_with(token) {
            items.push(CompletionItem {
                replace: Some(replace),
                detail: Some(desc.into()),
                ..CompletionItem::new(fn_name, CompletionKind::Function)
            });
        }
    }
    items
}

/// Suggests relative YAML file and Kustomize directory paths from `base_dir` for `kustomization.yaml`.
fn push_kustomize_path_completions(
    index: &ProjectIndex,
    doc_norm_path: &str,
    base_dir: &str,
    typed: &str,
    dirs_only: bool,
    replace: Range,
    items: &mut Vec<CompletionItem>,
) {
    let mut seen = BTreeSet::new();
    // 1. Kustomize directories (`base`, `components/*`, `overlays/*`)
    for kdir in &index.kustomize_dirs {
        if kdir == base_dir {
            continue;
        }
        if let Some(rel) = relative_path_between(base_dir, kdir)
            && rel != "."
            && rel.starts_with(typed)
            && seen.insert(rel.clone())
        {
            items.push(CompletionItem {
                replace: Some(replace),
                detail: Some("Kustomize directory".into()),
                ..CompletionItem::new(&rel, CompletionKind::Folder)
            });
        }
    }
    if dirs_only {
        return;
    }
    // 2. Sibling and child YAML files
    for path in index.files.keys() {
        if path == doc_norm_path {
            continue;
        }
        let fname = path.rsplit('/').next().unwrap_or(path);
        if matches!(
            fname,
            "kustomization.yaml" | "kustomization.yml" | "Kustomization"
        ) {
            continue;
        }
        let pdir = parent_dir(path);
        if pdir != base_dir && !pdir.starts_with(&format!("{base_dir}/")) {
            continue;
        }
        if let Some(rel) = relative_path_between(base_dir, path)
            && rel.starts_with(typed)
            && seen.insert(rel.clone())
        {
            items.push(CompletionItem {
                replace: Some(replace),
                detail: Some("Workspace manifest".into()),
                ..CompletionItem::new(&rel, CompletionKind::File)
            });
        }
    }
}

/// Computes completions at `at` in `doc`.
pub fn completions_at(
    index: &ProjectIndex,
    doc: &Document,
    at: Position,
    parsed: &ParsedFile,
) -> Vec<CompletionItem> {
    let norm_path = normalize_slashes(&doc.path);
    let fname = norm_path.rsplit('/').next().unwrap_or(&norm_path);
    let base_dir = parent_dir(&norm_path);

    // 1. Cursor inside an open `{{ ... }}` Go/Helm template action
    if let Some(helm_ctx) = helm_action_context_at(&doc.text, at) {
        return helm_action_completions(index, &norm_path, at, &helm_ctx);
    }

    let (parent_path, current_line, doc_kind) = yaml_context_at(&doc.text, at);
    let col_utf16 = at.character as usize;
    let units: Vec<u16> = current_line.encode_utf16().take(col_utf16).collect();
    let line_prefix = String::from_utf16_lossy(&units);
    let mut items = Vec::new();

    // Fallback inline `.Values.` / `include "` check when line has no `{{` (e.g. partial edit)
    if let Some(idx) = line_prefix
        .rfind("include \"")
        .or_else(|| line_prefix.rfind("template \""))
    {
        let quote_pos = line_prefix[idx..].find('"').unwrap_or(0) + idx + 1;
        let typed = &line_prefix[quote_pos..];
        if !typed.contains('"') {
            let ctx = HelmActionContext {
                action_prefix: line_prefix[idx..].to_owned(),
                dot_scope: None,
                vars: BTreeMap::new(),
            };
            return helm_action_completions(index, &norm_path, at, &ctx);
        }
    }
    if let Some(idx) = line_prefix.rfind(".Values.") {
        let typed = &line_prefix[idx + ".Values.".len()..];
        if typed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        {
            push_helm_values_completions(index, &norm_path, at, "", typed, true, &mut items);
            return items;
        }
    }

    if fname.ends_with(".tpl") || fname.ends_with(".gotmpl") {
        return items;
    }

    let trimmed_prefix = line_prefix.trim_start();
    let is_chart_yaml = matches!(fname, "Chart.yaml" | "Chart.yml");
    let (_, kustomize_helm_vals, _) = index.collect_kustomize_targets();
    let is_values_yaml = is_helm_values_filename(fname) || kustomize_helm_vals.contains(&norm_path);
    let is_kustomization = ProjectIndex::is_kustomization_file(fname, doc_kind.as_deref());
    let is_json6902_patch = parsed.has_json6902_ops
        || (index.is_kustomize_patch_path(&norm_path)
            && doc_kind.is_none()
            && parsed.docs.is_empty()
            && (trimmed_prefix.starts_with('-') || parent_path.is_empty()));

    // Role A: Helm `Chart.yaml`
    if is_chart_yaml {
        if let Some(colon_idx) = trimmed_prefix.find(':') {
            let key = trimmed_prefix[..colon_idx]
                .trim()
                .trim_start_matches("- ")
                .trim();
            let raw_val = trimmed_prefix[colon_idx + 1..].trim_start();
            let start_col = at.character.saturating_sub(utf16_len(raw_val));
            let replace = Range::new(Position::new(at.line, start_col), at);
            match (parent_path.as_str(), key) {
                ("", "apiVersion") => {
                    for (v, desc) in [
                        ("v2", "Helm 3 chart API version"),
                        ("v1", "Legacy Helm 2 chart API version"),
                    ] {
                        if v.starts_with(raw_val) {
                            items.push(CompletionItem {
                                replace: Some(replace),
                                detail: Some(desc.into()),
                                ..CompletionItem::new(v, CompletionKind::Value)
                            });
                        }
                    }
                }
                ("", "type") => {
                    for (t, desc) in [
                        ("application", "Standard installable Helm chart"),
                        ("library", "Reusable helper/template library chart"),
                    ] {
                        if t.starts_with(raw_val) {
                            items.push(CompletionItem {
                                replace: Some(replace),
                                detail: Some(desc.into()),
                                ..CompletionItem::new(t, CompletionKind::EnumMember)
                            });
                        }
                    }
                }
                _ => {}
            }
            return items;
        }
        let typed_key = trimmed_prefix.trim_start_matches("- ");
        let start_col = at.character.saturating_sub(utf16_len(typed_key));
        let replace = Range::new(Position::new(at.line, start_col), at);
        for (field, snippet, detail) in chart_yaml_keys_for(&parent_path) {
            if field.starts_with(typed_key) {
                items.push(snippet_item(field, *snippet, replace, detail));
            }
        }
        return items;
    }

    // Role B: Helm `values.yaml` / `values-*.yaml` — suggest keys referenced by `.Values.*` in chart templates
    if is_values_yaml {
        if trimmed_prefix.contains(':') {
            return items;
        }
        let typed_key = trimmed_prefix.trim_start_matches("- ");
        let start_col = at.character.saturating_sub(utf16_len(typed_key));
        let replace = Range::new(Position::new(at.line, start_col), at);
        if let Some(chart_root) = index.infer_helm_chart_dir(&norm_path) {
            let mut seen = BTreeSet::new();
            let prefix_dot = if parent_path.is_empty() {
                String::new()
            } else {
                format!("{parent_path}.")
            };
            for (tpath, tfile) in &index.files {
                if tpath == &norm_path {
                    continue;
                }
                let tdir = parent_dir(tpath);
                if tdir != chart_root && !tdir.starts_with(&format!("{chart_root}/")) {
                    continue;
                }
                let tfname = tpath.rsplit('/').next().unwrap_or(tpath);
                for href in &tfile.helm_value_refs {
                    let Some(rem) = (if prefix_dot.is_empty() {
                        Some(href.value.as_str())
                    } else {
                        href.value.strip_prefix(&prefix_dot)
                    }) else {
                        continue;
                    };
                    let seg = rem.split('.').next().unwrap_or(rem);
                    if !seg.is_empty() && seg.starts_with(typed_key) && seen.insert(seg.to_owned())
                    {
                        let has_children = rem.contains('.');
                        let snip = if has_children {
                            format!("{seg}:\n  $0")
                        } else {
                            format!("{seg}: ${{1:value}}")
                        };
                        items.push(snippet_item(
                            seg,
                            snip,
                            replace,
                            &format!("Referenced in {tfname}"),
                        ));
                    }
                }
            }
        }
        return items;
    }

    // Role C: Kustomize `kustomization.yaml` / `Component`
    if is_kustomization {
        // Scalar list item under `resources:`, `components:`, `patches:`, etc.
        if let Some(after_dash) = trimmed_prefix.strip_prefix('-')
            && !after_dash.contains(':')
        {
            let raw_val = after_dash.trim_start();
            if matches!(
                parent_path.as_str(),
                "resources"
                    | "components"
                    | "bases"
                    | "patches"
                    | "patchesStrategicMerge"
                    | "patchesJson6902"
                    | "configurations"
                    | "generators"
                    | "transformers"
                    | "validators"
                    | "helmCharts.additionalValuesFiles"
            ) {
                let start_col = at.character.saturating_sub(utf16_len(raw_val));
                let replace = Range::new(Position::new(at.line, start_col), at);
                let dirs_only = matches!(parent_path.as_str(), "components" | "bases");
                push_kustomize_path_completions(
                    index, &norm_path, &base_dir, raw_val, dirs_only, replace, &mut items,
                );
                return items;
            }
        }

        if let Some(colon_idx) = trimmed_prefix.find(':') {
            let key = trimmed_prefix[..colon_idx]
                .trim()
                .trim_start_matches("- ")
                .trim();
            let raw_val = trimmed_prefix[colon_idx + 1..].trim_start();
            let start_col = at.character.saturating_sub(utf16_len(raw_val));
            let replace = Range::new(Position::new(at.line, start_col), at);
            let full_key = if parent_path.is_empty() {
                key.to_owned()
            } else {
                format!("{parent_path}.{key}")
            };
            match full_key.as_str() {
                "apiVersion" => {
                    for (v, desc) in [
                        ("kustomize.config.k8s.io/v1beta1", "Kustomization API"),
                        (
                            "kustomize.config.k8s.io/v1alpha1",
                            "Kustomize Component API",
                        ),
                    ] {
                        if v.starts_with(raw_val) {
                            items.push(CompletionItem {
                                replace: Some(replace),
                                detail: Some(desc.into()),
                                ..CompletionItem::new(v, CompletionKind::Value)
                            });
                        }
                    }
                }
                "kind" => {
                    for (k, desc) in [
                        ("Kustomization", "Kustomize overlay / base"),
                        ("Component", "Reusable Kustomize component"),
                    ] {
                        if k.to_ascii_lowercase()
                            .starts_with(&raw_val.to_ascii_lowercase())
                        {
                            items.push(CompletionItem {
                                replace: Some(replace),
                                detail: Some(desc.into()),
                                ..CompletionItem::new(k, CompletionKind::Class)
                            });
                        }
                    }
                }
                "patches.path"
                | "patchesJson6902.path"
                | "replacements.path"
                | "helmCharts.valuesFile"
                | "openapi.path" => {
                    push_kustomize_path_completions(
                        index, &norm_path, &base_dir, raw_val, false, replace, &mut items,
                    );
                }
                "images.name" => {
                    let mut seen = BTreeSet::new();
                    for (_, kdoc) in index.all_docs() {
                        for c in &kdoc.containers {
                            if let Some(ref img) = c.image
                                && !img.value.contains("{{")
                            {
                                let repo = img.value.split(':').next().unwrap_or(&img.value);
                                if repo.starts_with(raw_val) && seen.insert(repo.to_owned()) {
                                    items.push(CompletionItem {
                                        replace: Some(replace),
                                        detail: Some(format!("Container image ({})", img.value)),
                                        ..CompletionItem::new(repo, CompletionKind::Value)
                                    });
                                }
                            }
                        }
                    }
                }
                "replicas.name" | "patches.target.name" | "patchesJson6902.target.name" => {
                    let mut seen = BTreeSet::new();
                    for (p, kdoc) in index.all_docs() {
                        if let Some(ref n) = kdoc.name
                            && !n.value.contains("{{")
                            && n.value.starts_with(raw_val)
                            && seen.insert(n.value.clone())
                        {
                            let k = kdoc.kind.as_ref().map_or("Resource", |k| k.value.as_str());
                            let pf = p.rsplit('/').next().unwrap_or(p);
                            items.push(CompletionItem {
                                replace: Some(replace),
                                detail: Some(format!("{k} in {pf}")),
                                ..CompletionItem::new(&n.value, CompletionKind::Reference)
                            });
                        }
                    }
                }
                "patches.target.kind" | "patchesJson6902.target.kind" => {
                    for &(k, desc) in KINDS {
                        if k != "Kustomization"
                            && k.to_ascii_lowercase()
                                .starts_with(&raw_val.to_ascii_lowercase())
                        {
                            items.push(CompletionItem {
                                replace: Some(replace),
                                detail: Some(desc.into()),
                                ..CompletionItem::new(k, CompletionKind::Class)
                            });
                        }
                    }
                }
                "configMapGenerator.behavior" | "secretGenerator.behavior" => {
                    for b in ["create", "merge", "replace"] {
                        if b.starts_with(raw_val) {
                            items.push(CompletionItem {
                                replace: Some(replace),
                                detail: Some("Generator merge behavior".into()),
                                ..CompletionItem::new(b, CompletionKind::EnumMember)
                            });
                        }
                    }
                }
                _ => {}
            }
            return items;
        }

        let typed_key = trimmed_prefix
            .trim_start_matches("- ")
            .trim_start_matches('-')
            .trim_start();
        let start_col = at.character.saturating_sub(utf16_len(typed_key));
        let replace = Range::new(Position::new(at.line, start_col), at);
        for (field, snippet, detail) in kustomize_keys_for(&parent_path) {
            if field.starts_with(typed_key) {
                items.push(snippet_item(field, *snippet, replace, detail));
            }
        }
        return items;
    }

    // Role D: JSON 6902 Patch file (`- op: replace`, `path: /spec/replicas`, `value: ...`)
    if is_json6902_patch {
        if let Some(colon_idx) = trimmed_prefix.find(':') {
            let key = trimmed_prefix[..colon_idx]
                .trim()
                .trim_start_matches("- ")
                .trim();
            let raw_val = trimmed_prefix[colon_idx + 1..].trim_start();
            let start_col = at.character.saturating_sub(utf16_len(raw_val));
            let replace = Range::new(Position::new(at.line, start_col), at);
            if key == "op" {
                for (op, desc) in [
                    ("replace", "Replace existing field value"),
                    ("add", "Add field or array element"),
                    ("remove", "Remove field or array element"),
                    ("move", "Move value from `from` to `path`"),
                    ("copy", "Copy value from `from` to `path`"),
                    ("test", "Assert value at `path` equals `value`"),
                ] {
                    if op.starts_with(raw_val) {
                        items.push(CompletionItem {
                            replace: Some(replace),
                            detail: Some(desc.into()),
                            ..CompletionItem::new(op, CompletionKind::EnumMember)
                        });
                    }
                }
            }
            return items;
        }
        let typed_key = trimmed_prefix
            .trim_start_matches("- ")
            .trim_start_matches('-')
            .trim_start();
        let start_col = at.character.saturating_sub(utf16_len(typed_key));
        let replace = Range::new(Position::new(at.line, start_col), at);
        for (field, snippet, detail) in [
            ("op", "op: ${1:replace}", "JSON 6902 patch operation"),
            (
                "path",
                "path: ${1:/spec/replicas}",
                "JSON Pointer target path",
            ),
            ("value", "value: ${1:value}", "New or expected value"),
            (
                "from",
                "from: ${1:/spec/source}",
                "Source JSON Pointer (move/copy)",
            ),
        ] {
            if field.starts_with(typed_key) {
                items.push(snippet_item(field, snippet, replace, detail));
            }
        }
        return items;
    }

    // Role E: Kubernetes resource manifest, Helm YAML template, or Strategic Merge Patch
    // 2. Value completion after `key: `
    if let Some(colon_idx) = trimmed_prefix.find(':') {
        let key = trimmed_prefix[..colon_idx]
            .trim()
            .trim_start_matches("- ")
            .trim();
        let raw_val = trimmed_prefix[colon_idx + 1..].trim_start();
        let start_col = at.character.saturating_sub(utf16_len(raw_val));
        let replace = Range::new(Position::new(at.line, start_col), at);
        let full_key = if parent_path.is_empty() {
            key.to_owned()
        } else {
            format!("{parent_path}.{key}")
        };

        match full_key.as_str() {
            "apiVersion" => {
                let mut seen = BTreeSet::new();
                for &(ver, desc) in API_VERSIONS {
                    if ver.starts_with(raw_val) && seen.insert(ver.to_owned()) {
                        items.push(CompletionItem {
                            replace: Some(replace),
                            detail: Some(desc.into()),
                            ..CompletionItem::new(ver, CompletionKind::Value)
                        });
                    }
                }
                for (_, kdoc) in index.all_docs() {
                    if let Some(ref crd) = kdoc.crd {
                        for v in &crd.versions {
                            let group_ver = format!("{}/{v}", crd.group);
                            if group_ver.starts_with(raw_val) && seen.insert(group_ver.clone()) {
                                items.push(CompletionItem {
                                    replace: Some(replace),
                                    detail: Some(format!("CRD API ({})", crd.kind.value)),
                                    ..CompletionItem::new(&group_ver, CompletionKind::Value)
                                });
                            }
                        }
                    }
                }
                return items;
            }
            "kind" => {
                for &(k, desc) in KINDS {
                    if k.to_ascii_lowercase()
                        .starts_with(&raw_val.to_ascii_lowercase())
                    {
                        items.push(CompletionItem {
                            replace: Some(replace),
                            detail: Some(desc.into()),
                            ..CompletionItem::new(k, CompletionKind::Class)
                        });
                    }
                }
                for (path, kdoc) in index.all_docs() {
                    if let Some(ref crd) = kdoc.crd
                        && !crd.kind.value.is_empty()
                        && crd
                            .kind
                            .value
                            .to_ascii_lowercase()
                            .starts_with(&raw_val.to_ascii_lowercase())
                    {
                        let pfname = path.rsplit('/').next().unwrap_or(path);
                        items.push(CompletionItem {
                            replace: Some(replace),
                            detail: Some(format!("CRD ({}) in {pfname}", crd.group)),
                            ..CompletionItem::new(&crd.kind.value, CompletionKind::Class)
                        });
                    }
                }
                return items;
            }
            "type" if doc_kind.as_deref() == Some("Secret") => {
                for (st, desc) in [
                    ("Opaque", "Arbitrary user-defined data"),
                    ("kubernetes.io/tls", "TLS certificate and private key"),
                    (
                        "kubernetes.io/dockerconfigjson",
                        "Docker registry credentials",
                    ),
                    (
                        "kubernetes.io/basic-auth",
                        "Basic authentication credentials",
                    ),
                    ("kubernetes.io/ssh-auth", "SSH private key credentials"),
                    (
                        "kubernetes.io/service-account-token",
                        "ServiceAccount token",
                    ),
                ] {
                    if st.starts_with(raw_val) {
                        items.push(CompletionItem {
                            replace: Some(replace),
                            detail: Some(desc.into()),
                            ..CompletionItem::new(st, CompletionKind::EnumMember)
                        });
                    }
                }
                return items;
            }
            "spec.type" if doc_kind.as_deref() == Some("Service") => {
                for st in ["ClusterIP", "NodePort", "LoadBalancer", "ExternalName"] {
                    if st.starts_with(raw_val) {
                        items.push(CompletionItem {
                            replace: Some(replace),
                            detail: Some("Service type".into()),
                            ..CompletionItem::new(st, CompletionKind::EnumMember)
                        });
                    }
                }
                return items;
            }
            "spec.concurrencyPolicy" if doc_kind.as_deref() == Some("CronJob") => {
                for cp in ["Allow", "Forbid", "Replace"] {
                    if cp.starts_with(raw_val) {
                        items.push(CompletionItem {
                            replace: Some(replace),
                            detail: Some("CronJob concurrency policy".into()),
                            ..CompletionItem::new(cp, CompletionKind::EnumMember)
                        });
                    }
                }
                return items;
            }
            "spec.scaleTargetRef.kind" => {
                for wk in ["Deployment", "StatefulSet", "ReplicaSet"] {
                    if wk.starts_with(raw_val) {
                        items.push(CompletionItem {
                            replace: Some(replace),
                            detail: Some("Scalable workload kind".into()),
                            ..CompletionItem::new(wk, CompletionKind::Class)
                        });
                    }
                }
                return items;
            }
            "spec.scaleTargetRef.name" => {
                let active_doc = parsed
                    .docs
                    .iter()
                    .find(|d| d.start_line <= at.line && at.line <= d.end_line)
                    .or_else(|| parsed.docs.first());
                let wanted_kind = active_doc
                    .and_then(|d| d.scale_target_ref.as_ref())
                    .and_then(|(k, _)| k.as_deref());
                let mut seen = BTreeSet::new();
                for (path, kdoc) in index.all_docs() {
                    let k = kdoc.kind.as_ref().map(|s| s.value.as_str()).unwrap_or("");
                    let matches_kind = match wanted_kind {
                        Some(wk) if !wk.is_empty() => k == wk,
                        _ => matches!(k, "Deployment" | "StatefulSet" | "ReplicaSet"),
                    };
                    if matches_kind
                        && let Some(ref n) = kdoc.name
                        && !n.value.contains("{{")
                        && n.value.starts_with(raw_val)
                        && seen.insert(n.value.clone())
                    {
                        let pfname = path.rsplit('/').next().unwrap_or(path);
                        items.push(CompletionItem {
                            replace: Some(replace),
                            detail: Some(format!("{k} in {pfname}")),
                            ..CompletionItem::new(&n.value, CompletionKind::Reference)
                        });
                    }
                }
                return items;
            }
            "roleRef.kind" => {
                for rk in ["Role", "ClusterRole"] {
                    if rk.starts_with(raw_val) {
                        items.push(CompletionItem {
                            replace: Some(replace),
                            ..CompletionItem::new(rk, CompletionKind::Class)
                        });
                    }
                }
                return items;
            }
            "roleRef.name" => {
                let mut seen = BTreeSet::new();
                for (path, kdoc) in index.all_docs() {
                    let k = kdoc.kind.as_ref().map(|s| s.value.as_str()).unwrap_or("");
                    if matches!(k, "Role" | "ClusterRole")
                        && let Some(ref n) = kdoc.name
                        && n.value.starts_with(raw_val)
                        && seen.insert(n.value.clone())
                    {
                        let pfname = path.rsplit('/').next().unwrap_or(path);
                        items.push(CompletionItem {
                            replace: Some(replace),
                            detail: Some(format!("{k} in {pfname}")),
                            ..CompletionItem::new(&n.value, CompletionKind::Reference)
                        });
                    }
                }
                return items;
            }
            "subjects.kind" => {
                for sk in ["ServiceAccount", "User", "Group"] {
                    if sk.starts_with(raw_val) {
                        items.push(CompletionItem {
                            replace: Some(replace),
                            ..CompletionItem::new(sk, CompletionKind::Class)
                        });
                    }
                }
                return items;
            }
            _ => {}
        }

        if full_key.ends_with("volumeMounts.name") {
            let active_doc = parsed
                .docs
                .iter()
                .find(|d| d.start_line <= at.line && at.line <= d.end_line)
                .or_else(|| parsed.docs.first());
            if let Some(d) = active_doc {
                let mut seen = BTreeSet::new();
                for vol in &d.volumes {
                    if vol.value.starts_with(raw_val) && seen.insert(vol.value.clone()) {
                        items.push(CompletionItem {
                            replace: Some(replace),
                            detail: Some("Pod volume".into()),
                            ..CompletionItem::new(&vol.value, CompletionKind::Reference)
                        });
                    }
                }
            }
            return items;
        }

        if key == "imagePullPolicy" {
            for p in ["IfNotPresent", "Always", "Never"] {
                if p.starts_with(raw_val) {
                    items.push(CompletionItem {
                        replace: Some(replace),
                        ..CompletionItem::new(p, CompletionKind::EnumMember)
                    });
                }
            }
            return items;
        }
        if key == "restartPolicy" {
            for p in ["Always", "OnFailure", "Never"] {
                if p.starts_with(raw_val) {
                    items.push(CompletionItem {
                        replace: Some(replace),
                        ..CompletionItem::new(p, CompletionKind::EnumMember)
                    });
                }
            }
            return items;
        }
        if key == "pathType" {
            for p in ["Prefix", "Exact", "ImplementationSpecific"] {
                if p.starts_with(raw_val) {
                    items.push(CompletionItem {
                        replace: Some(replace),
                        ..CompletionItem::new(p, CompletionKind::EnumMember)
                    });
                }
            }
            return items;
        }
        if key == "protocol" {
            for p in ["TCP", "UDP", "SCTP"] {
                if p.starts_with(raw_val) {
                    items.push(CompletionItem {
                        replace: Some(replace),
                        ..CompletionItem::new(p, CompletionKind::EnumMember)
                    });
                }
            }
            return items;
        }

        // Cross-resource name completions
        if full_key.contains("configMapKeyRef.name")
            || full_key.contains("configMapRef.name")
            || full_key.ends_with("configMap.name")
        {
            for (path, kdoc) in index.all_docs() {
                if kdoc.kind.as_ref().map(|k| k.value.as_str()) == Some("ConfigMap")
                    && let Some(ref n) = kdoc.name
                    && !n.value.contains("{{")
                {
                    let pfname = path.rsplit('/').next().unwrap_or(path);
                    items.push(CompletionItem {
                        replace: Some(replace),
                        detail: Some(format!("ConfigMap in {pfname}")),
                        ..CompletionItem::new(&n.value, CompletionKind::Reference)
                    });
                }
            }
            return items;
        }

        if full_key.contains("secretKeyRef.name")
            || full_key.contains("secretRef.name")
            || full_key.ends_with("secret.secretName")
            || full_key.ends_with("tls.secretName")
            || full_key.ends_with("imagePullSecrets.name")
        {
            for (path, kdoc) in index.all_docs() {
                if kdoc.kind.as_ref().map(|k| k.value.as_str()) == Some("Secret")
                    && let Some(ref n) = kdoc.name
                    && !n.value.contains("{{")
                {
                    let pfname = path.rsplit('/').next().unwrap_or(path);
                    items.push(CompletionItem {
                        replace: Some(replace),
                        detail: Some(format!("Secret in {pfname}")),
                        ..CompletionItem::new(&n.value, CompletionKind::Reference)
                    });
                }
            }
            return items;
        }

        // ConfigMap / Secret `key:` completion based on the referenced ConfigMap/Secret in the current doc
        if full_key.ends_with("configMapKeyRef.key") || full_key.ends_with("secretKeyRef.key") {
            let target_kind = if full_key.contains("configMapKeyRef") {
                "ConfigMap"
            } else {
                "Secret"
            };
            let active_doc = parsed
                .docs
                .iter()
                .find(|d| d.start_line <= at.line && at.line <= d.end_line)
                .or_else(|| parsed.docs.first());
            let referenced_names: Vec<String> = active_doc
                .map(|d| {
                    d.data_refs
                        .iter()
                        .filter(|r| r.target_kind == target_kind)
                        .map(|r| r.name.value.clone())
                        .collect()
                })
                .unwrap_or_default();

            for (_, kdoc) in index.all_docs() {
                if kdoc.kind.as_ref().map(|k| k.value.as_str()) == Some(target_kind)
                    && let Some(ref n) = kdoc.name
                    && (referenced_names.is_empty() || referenced_names.contains(&n.value))
                {
                    for (dk, dv) in &kdoc.data_keys {
                        items.push(CompletionItem {
                            replace: Some(replace),
                            detail: Some(format!("{target_kind} '{}' ({})", n.value, dv.value)),
                            ..CompletionItem::new(dk, CompletionKind::Property)
                        });
                    }
                }
            }
            return items;
        }

        if full_key.ends_with("backend.service.name") {
            for (path, kdoc) in index.all_docs() {
                if kdoc.kind.as_ref().map(|k| k.value.as_str()) == Some("Service")
                    && let Some(ref n) = kdoc.name
                    && !n.value.contains("{{")
                {
                    let pfname = path.rsplit('/').next().unwrap_or(path);
                    items.push(CompletionItem {
                        replace: Some(replace),
                        detail: Some(format!("Service in {pfname}")),
                        ..CompletionItem::new(&n.value, CompletionKind::Reference)
                    });
                }
            }
            return items;
        }

        if full_key.ends_with("serviceAccountName") || full_key.ends_with("serviceAccount") {
            for (path, kdoc) in index.all_docs() {
                if kdoc.kind.as_ref().map(|k| k.value.as_str()) == Some("ServiceAccount")
                    && let Some(ref n) = kdoc.name
                    && !n.value.contains("{{")
                {
                    let pfname = path.rsplit('/').next().unwrap_or(path);
                    items.push(CompletionItem {
                        replace: Some(replace),
                        detail: Some(format!("ServiceAccount in {pfname}")),
                        ..CompletionItem::new(&n.value, CompletionKind::Reference)
                    });
                }
            }
            return items;
        }

        if full_key.ends_with("persistentVolumeClaim.claimName") {
            for (path, kdoc) in index.all_docs() {
                if kdoc.kind.as_ref().map(|k| k.value.as_str()) == Some("PersistentVolumeClaim")
                    && let Some(ref n) = kdoc.name
                    && !n.value.contains("{{")
                {
                    let pfname = path.rsplit('/').next().unwrap_or(path);
                    items.push(CompletionItem {
                        replace: Some(replace),
                        detail: Some(format!("PVC in {pfname}")),
                        ..CompletionItem::new(&n.value, CompletionKind::Reference)
                    });
                }
            }
            return items;
        }

        if full_key.ends_with("storageClassName") {
            for (path, kdoc) in index.all_docs() {
                if kdoc.kind.as_ref().map(|k| k.value.as_str()) == Some("StorageClass")
                    && let Some(ref n) = kdoc.name
                    && !n.value.contains("{{")
                {
                    let pfname = path.rsplit('/').next().unwrap_or(path);
                    items.push(CompletionItem {
                        replace: Some(replace),
                        detail: Some(format!("StorageClass in {pfname}")),
                        ..CompletionItem::new(&n.value, CompletionKind::Reference)
                    });
                }
            }
            return items;
        }

        if key == "targetPort" {
            let mut seen = BTreeSet::new();
            for (_, kdoc) in index.all_docs() {
                for c in &kdoc.containers {
                    for p in &c.ports {
                        if seen.insert(p.value.clone()) {
                            let wname = kdoc.name.as_ref().map_or("workload", |n| n.value.as_str());
                            items.push(CompletionItem {
                                replace: Some(replace),
                                detail: Some(format!("Container port in {wname}")),
                                ..CompletionItem::new(&p.value, CompletionKind::Value)
                            });
                        }
                    }
                }
            }
            return items;
        }
        return items;
    }

    // 3. Key and live-template completions when typing a key at `parent_path`
    let typed_key = trimmed_prefix
        .trim_start_matches("- ")
        .trim_start_matches('-')
        .trim_start();
    let start_col = at.character.saturating_sub(utf16_len(typed_key));
    let replace = Range::new(Position::new(at.line, start_col), at);

    // Selector label key:value pairs from workloads in the workspace
    if parent_path == "spec.selector" || parent_path == "spec.selector.matchLabels" {
        let mut seen = BTreeSet::new();
        for (_, kdoc) in index.all_docs() {
            if kdoc.is_workload() {
                for (lk, lv) in kdoc.target_labels() {
                    if lv.value.contains("{{") {
                        continue;
                    }
                    let pair = format!("{lk}: {}", lv.value);
                    if pair.starts_with(typed_key) && seen.insert(pair.clone()) {
                        let wname = kdoc.name.as_ref().map_or("workload", |n| n.value.as_str());
                        items.push(CompletionItem {
                            replace: Some(replace),
                            detail: Some(format!("Label from {wname}")),
                            ..CompletionItem::new(&pair, CompletionKind::Property)
                        });
                    }
                }
            }
        }
    }

    for (field, snippet, detail) in schema_keys_for(&parent_path, doc_kind.as_deref()) {
        if field.starts_with(typed_key) {
            items.push(snippet_item(field, *snippet, replace, detail));
        }
    }

    // CustomResourceDefinition (CRD) `spec` property completion
    if parent_path == "spec"
        && let Some(ref k) = doc_kind
    {
        for (_, crd, _) in index.find_crd(k) {
            for prop in &crd.spec_properties {
                if prop.starts_with(typed_key) {
                    items.push(snippet_item(
                        prop,
                        format!("{prop}: ${{1:value}}"),
                        replace,
                        &format!("CRD {k} spec property"),
                    ));
                }
            }
        }
    }

    // Root-level IntelliJ-style Kubernetes live templates (`kdep`, `ksvc`, `kcm`, `ksec`, `king`, `kpod`, `kust`)
    if parent_path.is_empty() {
        for (abbr, title, body) in live_templates() {
            if abbr.starts_with(typed_key)
                || title
                    .to_ascii_lowercase()
                    .contains(&typed_key.to_ascii_lowercase())
            {
                let mut item = snippet_item(abbr, *body, replace, title);
                item.filter_text = Some(format!("{abbr} {title}"));
                items.push(item);
            }
        }
    }

    items
}

impl ProjectIndex {
    fn is_kustomization_file(fname: &str, doc_kind: Option<&str>) -> bool {
        matches!(
            fname,
            "kustomization.yaml" | "kustomization.yml" | "Kustomization"
        ) || matches!(doc_kind, Some("Kustomization" | "Component"))
    }
}

fn chart_yaml_keys_for(parent_path: &str) -> &'static [(&'static str, &'static str, &'static str)] {
    match parent_path {
        "" => &[
            (
                "apiVersion",
                "apiVersion: ${1:v2}",
                "Helm chart API version (v2)",
            ),
            ("name", "name: ${1:my-chart}", "Chart name"),
            ("version", "version: ${1:0.1.0}", "Chart SemVer 2 version"),
            (
                "appVersion",
                "appVersion: \"${1:1.0.0}\"",
                "Version of the packaged app",
            ),
            (
                "description",
                "description: ${1:A Helm chart for Kubernetes}",
                "Single-sentence description",
            ),
            (
                "type",
                "type: ${1:application}",
                "Chart type (application or library)",
            ),
            (
                "kubeVersion",
                "kubeVersion: \"${1:>= 1.26.0-0}\"",
                "SemVer range of compatible Kubernetes versions",
            ),
            (
                "keywords",
                "keywords:\n  - ${1:kubernetes}",
                "Search keywords",
            ),
            ("home", "home: ${1:https://example.com}", "Project home URL"),
            (
                "sources",
                "sources:\n  - ${1:https://github.com/org/repo}",
                "Source code URLs",
            ),
            (
                "dependencies",
                "dependencies:\n  - name: ${1:subchart}\n    version: \"${2:1.0.0}\"\n    repository: \"${3:https://charts.example.com}\"",
                "Chart dependencies",
            ),
            (
                "maintainers",
                "maintainers:\n  - name: ${1:Maintainer}\n    email: ${2:dev@example.com}",
                "Chart maintainers",
            ),
            (
                "icon",
                "icon: ${1:https://example.com/icon.svg}",
                "URL to SVG or PNG icon",
            ),
            (
                "deprecated",
                "deprecated: ${1:false}",
                "Whether this chart is deprecated",
            ),
            (
                "annotations",
                "annotations:\n  ${1:key}: \"${2:value}\"",
                "Custom chart annotations",
            ),
        ],
        "dependencies" => &[
            ("name", "name: ${1:subchart}", "Dependency chart name"),
            (
                "version",
                "version: \"${1:1.0.0}\"",
                "Dependency version or SemVer constraint",
            ),
            (
                "repository",
                "repository: \"${1:https://charts.example.com}\"",
                "Chart repository URL or oci:// URI",
            ),
            (
                "condition",
                "condition: ${1:subchart.enabled}",
                "Boolean values path toggling dependency",
            ),
            (
                "tags",
                "tags:\n  - ${1:frontend}",
                "Tags enabling dependency group",
            ),
            (
                "alias",
                "alias: ${1:custom-name}",
                "Alias name for dependency chart",
            ),
            (
                "import-values",
                "import-values:\n  - ${1:data}",
                "Values keys imported from subchart",
            ),
        ],
        "maintainers" => &[
            ("name", "name: ${1:Maintainer}", "Maintainer name"),
            ("email", "email: ${1:dev@example.com}", "Maintainer email"),
            ("url", "url: ${1:https://github.com/user}", "Maintainer URL"),
        ],
        _ => &[],
    }
}

fn kustomize_keys_for(parent_path: &str) -> &'static [(&'static str, &'static str, &'static str)] {
    match parent_path {
        "" => &[
            (
                "apiVersion",
                "apiVersion: kustomize.config.k8s.io/v1beta1",
                "Kustomize API group and version",
            ),
            ("kind", "kind: Kustomization", "Kustomization or Component"),
            (
                "resources",
                "resources:\n  - ${1:deployment.yaml}",
                "Kubernetes manifests or Kustomize directories",
            ),
            (
                "components",
                "components:\n  - ${1:../components/feature}",
                "Reusable Kustomize components",
            ),
            (
                "patches",
                "patches:\n  - path: ${1:patch-deployment.yaml}",
                "Strategic merge or JSON 6902 patches",
            ),
            (
                "namespace",
                "namespace: ${1:default}",
                "Namespace applied to all resources",
            ),
            (
                "namePrefix",
                "namePrefix: ${1:dev-}",
                "Prefix prepended to resource names",
            ),
            (
                "nameSuffix",
                "nameSuffix: ${1:-v1}",
                "Suffix appended to resource names",
            ),
            (
                "labels",
                "labels:\n  - pairs:\n      ${1:app.kubernetes.io/part-of}: ${2:my-app}\n    includeSelectors: false",
                "Labels added to resources",
            ),
            (
                "commonAnnotations",
                "commonAnnotations:\n  ${1:key}: \"${2:value}\"",
                "Annotations added to all resources",
            ),
            (
                "images",
                "images:\n  - name: ${1:nginx}\n    newName: ${2:nginx}\n    newTag: \"${3:1.27-alpine}\"",
                "Container image overrides",
            ),
            (
                "replicas",
                "replicas:\n  - name: ${1:my-app}\n    count: ${2:3}",
                "Workload replica count overrides",
            ),
            (
                "configMapGenerator",
                "configMapGenerator:\n  - name: ${1:app-config}\n    literals:\n      - ${2:KEY=value}",
                "Generate versioned ConfigMaps",
            ),
            (
                "secretGenerator",
                "secretGenerator:\n  - name: ${1:app-secret}\n    literals:\n      - ${2:SECRET=value}",
                "Generate versioned Secrets",
            ),
            (
                "generatorOptions",
                "generatorOptions:\n  disableNameSuffixHash: ${1:false}",
                "Options for generated ConfigMaps and Secrets",
            ),
            (
                "replacements",
                "replacements:\n  - path: ${1:replacement.yaml}",
                "Field-to-field value replacements",
            ),
            (
                "helmCharts",
                "helmCharts:\n  - name: ${1:my-chart}\n    repo: ${2:https://charts.example.com}\n    version: ${3:1.0.0}\n    releaseName: ${4:my-release}\n    valuesFile: ${5:values.yaml}",
                "Inflate Helm charts in Kustomize",
            ),
            (
                "configurations",
                "configurations:\n  - ${1:kustomizeconfig.yaml}",
                "Custom transformer configurations",
            ),
            (
                "generators",
                "generators:\n  - ${1:generator.yaml}",
                "KRM generator plugins",
            ),
            (
                "transformers",
                "transformers:\n  - ${1:transformer.yaml}",
                "KRM transformer plugins",
            ),
            (
                "validators",
                "validators:\n  - ${1:validator.yaml}",
                "KRM validator plugins",
            ),
        ],
        "patches" | "patchesJson6902" => &[
            (
                "path",
                "path: ${1:patch.yaml}",
                "Relative path to patch file",
            ),
            (
                "patch",
                "patch: |-\n  ${1:- op: replace\n    path: /spec/replicas\n    value: 3}",
                "Inline strategic merge or JSON 6902 patch",
            ),
            (
                "target",
                "target:\n  kind: ${1:Deployment}\n  name: ${2:my-app}",
                "Target resource selector for patch",
            ),
            (
                "options",
                "options:\n  allowNameChange: ${1:false}",
                "Patch options",
            ),
        ],
        "patches.target" | "patchesJson6902.target" => &[
            ("group", "group: ${1:apps}", "Target API group"),
            ("version", "version: ${1:v1}", "Target API version"),
            ("kind", "kind: ${1:Deployment}", "Target resource kind"),
            ("name", "name: ${1:my-app}", "Target resource name or regex"),
            (
                "namespace",
                "namespace: ${1:default}",
                "Target resource namespace",
            ),
            (
                "labelSelector",
                "labelSelector: \"${1:app=my-app}\"",
                "Label selector filter",
            ),
            (
                "annotationSelector",
                "annotationSelector: \"${1:key=val}\"",
                "Annotation selector filter",
            ),
        ],
        "images" => &[
            (
                "name",
                "name: ${1:nginx}",
                "Original image repository to match",
            ),
            (
                "newName",
                "newName: ${1:ghcr.io/org/app}",
                "Replacement image repository",
            ),
            ("newTag", "newTag: \"${1:1.0.0}\"", "Replacement image tag"),
            (
                "digest",
                "digest: ${1:sha256:...}",
                "Immutable image digest",
            ),
        ],
        "replicas" => &[
            ("name", "name: ${1:my-app}", "Target workload name"),
            ("count", "count: ${1:3}", "Desired replica count"),
        ],
        "configMapGenerator" | "secretGenerator" => &[
            ("name", "name: ${1:app-config}", "Generated resource name"),
            (
                "namespace",
                "namespace: ${1:default}",
                "Generated resource namespace",
            ),
            (
                "behavior",
                "behavior: ${1:create}",
                "Merge behavior (create, merge, replace)",
            ),
            (
                "literals",
                "literals:\n  - ${1:KEY=value}",
                "Inline KEY=VALUE entries",
            ),
            (
                "files",
                "files:\n  - ${1:config.properties}",
                "Data files to embed",
            ),
            ("envs", "envs:\n  - ${1:.env}", "Dotenv files to load"),
            ("type", "type: ${1:Opaque}", "Secret type"),
            (
                "options",
                "options:\n  disableNameSuffixHash: ${1:false}",
                "Per-generator options",
            ),
        ],
        "generatorOptions" | "configMapGenerator.options" | "secretGenerator.options" => &[
            (
                "disableNameSuffixHash",
                "disableNameSuffixHash: ${1:true}",
                "Omit content hash suffix from name",
            ),
            (
                "labels",
                "labels:\n  ${1:key}: ${2:value}",
                "Labels on generated resources",
            ),
            (
                "annotations",
                "annotations:\n  ${1:key}: \"${2:value}\"",
                "Annotations on generated resources",
            ),
            (
                "immutable",
                "immutable: ${1:true}",
                "Mark generated ConfigMap/Secret immutable",
            ),
        ],
        "labels" => &[
            (
                "pairs",
                "pairs:\n  ${1:app}: ${2:my-app}",
                "Label key-value pairs",
            ),
            (
                "includeSelectors",
                "includeSelectors: ${1:false}",
                "Also update selector.matchLabels",
            ),
            (
                "includeTemplates",
                "includeTemplates: ${1:true}",
                "Also update spec.template.metadata.labels",
            ),
        ],
        "helmCharts" => &[
            ("name", "name: ${1:my-chart}", "Helm chart name"),
            (
                "repo",
                "repo: ${1:https://charts.example.com}",
                "Helm repository URL",
            ),
            ("version", "version: \"${1:1.0.0}\"", "Chart version"),
            (
                "releaseName",
                "releaseName: ${1:my-release}",
                "Helm release name",
            ),
            ("namespace", "namespace: ${1:default}", "Release namespace"),
            (
                "valuesFile",
                "valuesFile: ${1:values.yaml}",
                "Values override file",
            ),
            (
                "additionalValuesFiles",
                "additionalValuesFiles:\n  - ${1:values-prod.yaml}",
                "Extra values files",
            ),
            (
                "valuesInline",
                "valuesInline:\n  ${1:replicaCount}: ${2:2}",
                "Inline values overrides",
            ),
            (
                "includeCRDs",
                "includeCRDs: ${1:false}",
                "Include chart CRDs",
            ),
        ],
        _ => &[],
    }
}

const POD_SPEC_KEYS: &[(&str, &str, &str)] = &[
    (
        "containers",
        "containers:\n  - name: ${1:app}\n    image: ${2:nginx:1.27-alpine}",
        "Application containers in the Pod",
    ),
    (
        "initContainers",
        "initContainers:\n  - name: ${1:init}\n    image: ${2:busybox:1.36}",
        "Initialization containers run before app containers",
    ),
    (
        "volumes",
        "volumes:\n  - name: ${1:config-vol}\n    configMap:\n      name: ${2:my-config}",
        "Pod storage volumes",
    ),
    (
        "serviceAccountName",
        "serviceAccountName: ${1:default}",
        "Pod ServiceAccount identity",
    ),
    (
        "automountServiceAccountToken",
        "automountServiceAccountToken: ${1:false}",
        "Mount ServiceAccount token into containers",
    ),
    (
        "restartPolicy",
        "restartPolicy: ${1:Always}",
        "Container restart policy (Always, OnFailure, Never)",
    ),
    (
        "imagePullSecrets",
        "imagePullSecrets:\n  - name: ${1:regcred}",
        "Registry pull Secrets",
    ),
    (
        "securityContext",
        "securityContext:\n  runAsNonRoot: true\n  seccompProfile:\n    type: RuntimeDefault",
        "Pod-level security context",
    ),
    (
        "nodeSelector",
        "nodeSelector:\n  ${1:kubernetes.io/os}: ${2:linux}",
        "Node label selector constraints",
    ),
    (
        "nodeName",
        "nodeName: ${1:worker-1}",
        "Pin Pod to a specific node",
    ),
    (
        "affinity",
        "affinity:\n  podAntiAffinity:\n    preferredDuringSchedulingIgnoredDuringExecution: []",
        "Node and Pod scheduling affinity rules",
    ),
    (
        "tolerations",
        "tolerations:\n  - key: \"${1:key}\"\n    operator: \"${2:Equal}\"\n    value: \"${3:value}\"\n    effect: \"${4:NoSchedule}\"",
        "Node taint tolerations",
    ),
    (
        "topologySpreadConstraints",
        "topologySpreadConstraints:\n  - maxSkew: 1\n    topologyKey: topology.kubernetes.io/zone\n    whenUnsatisfiable: DoNotSchedule",
        "Pod topology spread rules",
    ),
    (
        "dnsPolicy",
        "dnsPolicy: ${1:ClusterFirst}",
        "Pod DNS resolution policy",
    ),
    (
        "hostNetwork",
        "hostNetwork: ${1:false}",
        "Use host network namespace",
    ),
    (
        "terminationGracePeriodSeconds",
        "terminationGracePeriodSeconds: ${1:30}",
        "Graceful shutdown timeout in seconds",
    ),
    (
        "priorityClassName",
        "priorityClassName: ${1:system-cluster-critical}",
        "Pod scheduling priority class",
    ),
];

fn schema_keys_for(
    parent_path: &str,
    kind: Option<&str>,
) -> &'static [(&'static str, &'static str, &'static str)] {
    match parent_path {
        "" => match kind {
            Some("ConfigMap") => &[
                ("apiVersion", "apiVersion: v1", "API group and version"),
                ("kind", "kind: ConfigMap", "Resource kind"),
                (
                    "metadata",
                    "metadata:\n  name: ${1:my-config}",
                    "Standard object metadata",
                ),
                (
                    "data",
                    "data:\n  ${1:KEY}: \"${2:value}\"",
                    "UTF-8 configuration data entries",
                ),
                (
                    "binaryData",
                    "binaryData:\n  ${1:KEY}: \"${2:base64}\"",
                    "Base64 binary data entries",
                ),
                (
                    "immutable",
                    "immutable: ${1:true}",
                    "Prevent updates to ConfigMap data",
                ),
            ],
            Some("Secret") => &[
                ("apiVersion", "apiVersion: v1", "API group and version"),
                ("kind", "kind: Secret", "Resource kind"),
                (
                    "metadata",
                    "metadata:\n  name: ${1:my-secret}",
                    "Standard object metadata",
                ),
                (
                    "type",
                    "type: ${1:Opaque}",
                    "Secret type (Opaque, kubernetes.io/tls, ...)",
                ),
                (
                    "stringData",
                    "stringData:\n  ${1:KEY}: \"${2:value}\"",
                    "Plaintext secret entries",
                ),
                (
                    "data",
                    "data:\n  ${1:KEY}: \"${2:base64}\"",
                    "Base64-encoded secret entries",
                ),
                (
                    "immutable",
                    "immutable: ${1:true}",
                    "Prevent updates to Secret data",
                ),
            ],
            Some("ServiceAccount") => &[
                ("apiVersion", "apiVersion: v1", "API group and version"),
                ("kind", "kind: ServiceAccount", "Resource kind"),
                (
                    "metadata",
                    "metadata:\n  name: ${1:my-sa}",
                    "Standard object metadata",
                ),
                (
                    "automountServiceAccountToken",
                    "automountServiceAccountToken: ${1:true}",
                    "Auto-mount API credentials",
                ),
                (
                    "imagePullSecrets",
                    "imagePullSecrets:\n  - name: ${1:regcred}",
                    "Default image pull Secrets",
                ),
                (
                    "secrets",
                    "secrets:\n  - name: ${1:my-secret}",
                    "Referenced Secrets",
                ),
            ],
            Some("Role" | "ClusterRole") => &[
                (
                    "apiVersion",
                    "apiVersion: rbac.authorization.k8s.io/v1",
                    "RBAC API version",
                ),
                ("kind", "kind: ${1:Role}", "Role or ClusterRole"),
                (
                    "metadata",
                    "metadata:\n  name: ${1:my-role}",
                    "Standard object metadata",
                ),
                (
                    "rules",
                    "rules:\n  - apiGroups: [\"${1:}\"]\n    resources: [\"${2:pods}\"]\n    verbs: [\"${3:get}\", \"${4:list}\"]",
                    "RBAC policy rules",
                ),
                (
                    "aggregationRule",
                    "aggregationRule:\n  clusterRoleSelectors: []",
                    "ClusterRole aggregation",
                ),
            ],
            Some("RoleBinding" | "ClusterRoleBinding") => &[
                (
                    "apiVersion",
                    "apiVersion: rbac.authorization.k8s.io/v1",
                    "RBAC API version",
                ),
                (
                    "kind",
                    "kind: ${1:RoleBinding}",
                    "RoleBinding or ClusterRoleBinding",
                ),
                (
                    "metadata",
                    "metadata:\n  name: ${1:my-binding}",
                    "Standard object metadata",
                ),
                (
                    "roleRef",
                    "roleRef:\n  apiGroup: rbac.authorization.k8s.io\n  kind: ${1:Role}\n  name: ${2:my-role}",
                    "Referenced Role or ClusterRole",
                ),
                (
                    "subjects",
                    "subjects:\n  - kind: ${1:ServiceAccount}\n    name: ${2:default}\n    namespace: ${3:default}",
                    "Bound subjects",
                ),
            ],
            _ => &[
                (
                    "apiVersion",
                    "apiVersion: ${1:apps/v1}",
                    "API group and version",
                ),
                ("kind", "kind: ${1:Deployment}", "Resource kind"),
                (
                    "metadata",
                    "metadata:\n  name: ${1:my-app}",
                    "Standard object metadata",
                ),
                ("spec", "spec:\n  $0", "Desired state specification"),
                ("type", "type: ${1:Opaque}", "Secret / resource type"),
                (
                    "data",
                    "data:\n  ${1:KEY}: \"${2:value}\"",
                    "ConfigMap / Secret data entries",
                ),
                (
                    "stringData",
                    "stringData:\n  ${1:KEY}: \"${2:value}\"",
                    "Unencoded Secret string entries",
                ),
                (
                    "rules",
                    "rules:\n  - apiGroups: [\"\"]\n    resources: [\"pods\"]\n    verbs: [\"get\"]",
                    "RBAC policy rules",
                ),
                (
                    "roleRef",
                    "roleRef:\n  apiGroup: rbac.authorization.k8s.io\n  kind: Role\n  name: ${1:role}",
                    "RBAC role reference",
                ),
                (
                    "subjects",
                    "subjects:\n  - kind: ServiceAccount\n    name: ${1:default}",
                    "RBAC bound subjects",
                ),
            ],
        },
        p if p == "metadata" || p.ends_with(".metadata") => &[
            ("name", "name: ${1:my-app}", "Resource name"),
            ("namespace", "namespace: ${1:default}", "Target namespace"),
            (
                "labels",
                "labels:\n  app.kubernetes.io/name: ${1:my-app}",
                "Resource labels",
            ),
            (
                "annotations",
                "annotations:\n  ${1:key}: \"${2:value}\"",
                "Resource annotations",
            ),
            (
                "finalizers",
                "finalizers:\n  - ${1:finalizer.kubernetes.io}",
                "Deletion finalizers",
            ),
            (
                "generateName",
                "generateName: ${1:my-app-}",
                "Prefix for server-generated name",
            ),
        ],
        "spec.template.spec" | "spec.jobTemplate.spec.template.spec" => POD_SPEC_KEYS,
        "spec" => match kind {
            Some("Pod") => POD_SPEC_KEYS,
            Some("Service") => &[
                ("type", "type: ${1:ClusterIP}", "Service exposure type"),
                (
                    "selector",
                    "selector:\n  app.kubernetes.io/name: ${1:my-app}",
                    "Pod label selector",
                ),
                (
                    "ports",
                    "ports:\n  - name: ${1:http}\n    port: ${2:80}\n    targetPort: ${3:http}",
                    "Service port mappings",
                ),
                (
                    "clusterIP",
                    "clusterIP: ${1:None}",
                    "Cluster-internal IP or None (headless)",
                ),
                (
                    "externalTrafficPolicy",
                    "externalTrafficPolicy: ${1:Local}",
                    "External traffic routing policy",
                ),
                (
                    "sessionAffinity",
                    "sessionAffinity: ${1:ClientIP}",
                    "Client session stickiness",
                ),
                (
                    "externalName",
                    "externalName: ${1:db.example.com}",
                    "CNAME target for ExternalName service",
                ),
            ],
            Some("Ingress") => &[
                (
                    "ingressClassName",
                    "ingressClassName: ${1:nginx}",
                    "Ingress controller class",
                ),
                (
                    "defaultBackend",
                    "defaultBackend:\n  service:\n    name: ${1:default-svc}\n    port:\n      number: ${2:80}",
                    "Fallback backend when no rule matches",
                ),
                (
                    "rules",
                    "rules:\n  - host: ${1:example.com}\n    http:\n      paths:\n        - path: /\n          pathType: Prefix\n          backend:\n            service:\n              name: ${2:my-service}\n              port:\n                name: ${3:http}",
                    "Ingress routing rules",
                ),
                (
                    "tls",
                    "tls:\n  - hosts:\n      - ${1:example.com}\n    secretName: ${2:tls-secret}",
                    "TLS configuration",
                ),
            ],
            Some("HorizontalPodAutoscaler") => &[
                (
                    "scaleTargetRef",
                    "scaleTargetRef:\n  apiVersion: ${1:apps/v1}\n  kind: ${2:Deployment}\n  name: ${3:my-app}",
                    "Target workload to scale",
                ),
                (
                    "minReplicas",
                    "minReplicas: ${1:2}",
                    "Minimum replica count",
                ),
                (
                    "maxReplicas",
                    "maxReplicas: ${1:10}",
                    "Maximum replica count",
                ),
                (
                    "metrics",
                    "metrics:\n  - type: Resource\n    resource:\n      name: ${1:cpu}\n      target:\n        type: Utilization\n        averageUtilization: ${2:75}",
                    "Autoscaling metric specifications",
                ),
                (
                    "behavior",
                    "behavior:\n  scaleDown:\n    stabilizationWindowSeconds: ${1:300}",
                    "Scale-up and scale-down velocity rules",
                ),
            ],
            Some("StatefulSet") => &[
                ("replicas", "replicas: ${1:3}", "Desired replica count"),
                (
                    "serviceName",
                    "serviceName: ${1:my-headless}",
                    "Governing headless Service name",
                ),
                (
                    "selector",
                    "selector:\n  matchLabels:\n    app.kubernetes.io/name: ${1:my-app}",
                    "Label selector for Pods",
                ),
                (
                    "template",
                    "template:\n  metadata:\n    labels:\n      app.kubernetes.io/name: ${1:my-app}\n  spec:\n    containers:\n      - name: ${1:my-app}\n        image: ${2:nginx:1.27-alpine}",
                    "Pod template specification",
                ),
                (
                    "volumeClaimTemplates",
                    "volumeClaimTemplates:\n  - metadata:\n      name: ${1:data}\n    spec:\n      accessModes: [\"ReadWriteOnce\"]\n      resources:\n        requests:\n          storage: ${2:10Gi}",
                    "PersistentVolumeClaim templates per replica",
                ),
                (
                    "podManagementPolicy",
                    "podManagementPolicy: ${1:OrderedReady}",
                    "OrderedReady or Parallel",
                ),
                (
                    "updateStrategy",
                    "updateStrategy:\n  type: ${1:RollingUpdate}",
                    "StatefulSet rollout strategy",
                ),
            ],
            Some("DaemonSet") => &[
                (
                    "selector",
                    "selector:\n  matchLabels:\n    app.kubernetes.io/name: ${1:my-agent}",
                    "Label selector for Pods",
                ),
                (
                    "template",
                    "template:\n  metadata:\n    labels:\n      app.kubernetes.io/name: ${1:my-agent}\n  spec:\n    containers:\n      - name: ${1:my-agent}\n        image: ${2:busybox:1.36}",
                    "Pod template specification",
                ),
                (
                    "updateStrategy",
                    "updateStrategy:\n  type: ${1:RollingUpdate}",
                    "DaemonSet rollout strategy",
                ),
                (
                    "minReadySeconds",
                    "minReadySeconds: ${1:10}",
                    "Minimum seconds before Pod is ready",
                ),
            ],
            Some("Job") => &[
                (
                    "template",
                    "template:\n  spec:\n    restartPolicy: Never\n    containers:\n      - name: ${1:job}\n        image: ${2:busybox:1.36}",
                    "Pod template specification",
                ),
                (
                    "completions",
                    "completions: ${1:1}",
                    "Desired successful Pod completions",
                ),
                (
                    "parallelism",
                    "parallelism: ${1:1}",
                    "Max concurrent running Pods",
                ),
                (
                    "backoffLimit",
                    "backoffLimit: ${1:4}",
                    "Retries before marking Job failed",
                ),
                (
                    "activeDeadlineSeconds",
                    "activeDeadlineSeconds: ${1:600}",
                    "Max Job duration in seconds",
                ),
                (
                    "ttlSecondsAfterFinished",
                    "ttlSecondsAfterFinished: ${1:3600}",
                    "Auto-cleanup TTL after completion",
                ),
                ("suspend", "suspend: ${1:false}", "Suspend Job execution"),
            ],
            Some("CronJob") => &[
                (
                    "schedule",
                    "schedule: \"${1:*/5 * * * *}\"",
                    "Cron schedule expression",
                ),
                (
                    "timeZone",
                    "timeZone: \"${1:Etc/UTC}\"",
                    "IANA time zone for schedule",
                ),
                (
                    "concurrencyPolicy",
                    "concurrencyPolicy: ${1:Forbid}",
                    "Allow, Forbid, or Replace",
                ),
                (
                    "suspend",
                    "suspend: ${1:false}",
                    "Suspend future scheduled runs",
                ),
                (
                    "jobTemplate",
                    "jobTemplate:\n  spec:\n    template:\n      spec:\n        restartPolicy: OnFailure\n        containers:\n          - name: ${1:job}\n            image: ${2:busybox:1.36}",
                    "Template for created Jobs",
                ),
                (
                    "successfulJobsHistoryLimit",
                    "successfulJobsHistoryLimit: ${1:3}",
                    "Retained successful Jobs",
                ),
                (
                    "failedJobsHistoryLimit",
                    "failedJobsHistoryLimit: ${1:1}",
                    "Retained failed Jobs",
                ),
            ],
            Some("PersistentVolumeClaim") => &[
                (
                    "accessModes",
                    "accessModes:\n  - ${1:ReadWriteOnce}",
                    "Volume access modes",
                ),
                (
                    "resources",
                    "resources:\n  requests:\n    storage: ${1:10Gi}",
                    "Requested storage capacity",
                ),
                (
                    "storageClassName",
                    "storageClassName: ${1:standard}",
                    "StorageClass name",
                ),
                (
                    "volumeMode",
                    "volumeMode: ${1:Filesystem}",
                    "Filesystem or Block",
                ),
                (
                    "volumeName",
                    "volumeName: ${1:pv-name}",
                    "Bind to specific PersistentVolume",
                ),
            ],
            Some("NetworkPolicy") => &[
                (
                    "podSelector",
                    "podSelector:\n  matchLabels:\n    ${1:app}: ${2:my-app}",
                    "Target Pods in namespace",
                ),
                (
                    "policyTypes",
                    "policyTypes:\n  - ${1:Ingress}\n  - ${2:Egress}",
                    "Ingress and/or Egress enforcement",
                ),
                (
                    "ingress",
                    "ingress:\n  - from: []",
                    "Allowed inbound traffic rules",
                ),
                (
                    "egress",
                    "egress:\n  - to: []",
                    "Allowed outbound traffic rules",
                ),
            ],
            Some("PodDisruptionBudget") => &[
                (
                    "minAvailable",
                    "minAvailable: ${1:1}",
                    "Minimum available Pods or percentage",
                ),
                (
                    "maxUnavailable",
                    "maxUnavailable: ${1:1}",
                    "Maximum unavailable Pods or percentage",
                ),
                (
                    "selector",
                    "selector:\n  matchLabels:\n    ${1:app}: ${2:my-app}",
                    "Target Pod label selector",
                ),
            ],
            _ => &[
                ("replicas", "replicas: ${1:2}", "Desired replica count"),
                (
                    "selector",
                    "selector:\n  matchLabels:\n    app.kubernetes.io/name: ${1:my-app}",
                    "Label selector for Pods",
                ),
                (
                    "template",
                    "template:\n  metadata:\n    labels:\n      app.kubernetes.io/name: ${1:my-app}\n  spec:\n    containers:\n      - name: ${1:my-app}\n        image: ${2:nginx:1.27-alpine}",
                    "Pod template specification",
                ),
                (
                    "strategy",
                    "strategy:\n  type: ${1:RollingUpdate}",
                    "Deployment update strategy",
                ),
                (
                    "minReadySeconds",
                    "minReadySeconds: ${1:10}",
                    "Minimum seconds before Pod is ready",
                ),
                (
                    "revisionHistoryLimit",
                    "revisionHistoryLimit: ${1:10}",
                    "Old ReplicaSets to retain",
                ),
                (
                    "serviceAccountName",
                    "serviceAccountName: ${1:default}",
                    "Pod ServiceAccount name",
                ),
                (
                    "volumes",
                    "volumes:\n  - name: ${1:config-vol}\n    configMap:\n      name: ${2:my-config}",
                    "Pod storage volumes",
                ),
                (
                    "containers",
                    "containers:\n  - name: ${1:app}\n    image: ${2:nginx:1.27-alpine}",
                    "Pod containers list",
                ),
            ],
        },
        p if p.ends_with(".template") || p == "spec.jobTemplate" => &[
            (
                "metadata",
                "metadata:\n  labels:\n    ${1:app}: ${2:my-app}",
                "Template metadata",
            ),
            ("spec", "spec:\n  $0", "Template specification"),
        ],
        "spec.scaleTargetRef" => &[
            (
                "apiVersion",
                "apiVersion: ${1:apps/v1}",
                "Target workload API version",
            ),
            ("kind", "kind: ${1:Deployment}", "Target workload kind"),
            ("name", "name: ${1:my-app}", "Target workload name"),
        ],
        "spec.metrics" => &[
            (
                "type",
                "type: ${1:Resource}",
                "Metric source type (Resource, Pods, Object, External)",
            ),
            (
                "resource",
                "resource:\n  name: ${1:cpu}\n  target:\n    type: Utilization\n    averageUtilization: ${2:75}",
                "Container resource metric",
            ),
        ],
        "spec.metrics.resource" => &[
            ("name", "name: ${1:cpu}", "Resource name (cpu or memory)"),
            (
                "target",
                "target:\n  type: ${1:Utilization}\n  averageUtilization: ${2:75}",
                "Target metric threshold",
            ),
        ],
        "spec.metrics.resource.target" => &[
            (
                "type",
                "type: ${1:Utilization}",
                "Utilization, AverageValue, or Value",
            ),
            (
                "averageUtilization",
                "averageUtilization: ${1:75}",
                "Target percentage of requested resource",
            ),
            (
                "averageValue",
                "averageValue: ${1:500m}",
                "Target average value per Pod",
            ),
            ("value", "value: ${1:100}", "Target raw metric value"),
        ],
        "rules" if matches!(kind, Some("Role" | "ClusterRole")) => &[
            (
                "apiGroups",
                "apiGroups: [\"${1:}\"]",
                "API groups (\"\" for core API)",
            ),
            (
                "resources",
                "resources: [\"${1:pods}\"]",
                "Target resource plurals",
            ),
            (
                "verbs",
                "verbs: [\"${1:get}\", \"${2:list}\", \"${3:watch}\"]",
                "Allowed API verbs",
            ),
            (
                "resourceNames",
                "resourceNames: [\"${1:my-resource}\"]",
                "Restrict to named instances",
            ),
        ],
        "roleRef" => &[
            (
                "apiGroup",
                "apiGroup: rbac.authorization.k8s.io",
                "RBAC API group",
            ),
            ("kind", "kind: ${1:Role}", "Role or ClusterRole"),
            ("name", "name: ${1:my-role}", "Target Role/ClusterRole name"),
        ],
        "subjects" => &[
            (
                "kind",
                "kind: ${1:ServiceAccount}",
                "ServiceAccount, User, or Group",
            ),
            ("name", "name: ${1:default}", "Subject name"),
            (
                "namespace",
                "namespace: ${1:default}",
                "ServiceAccount namespace",
            ),
            (
                "apiGroup",
                "apiGroup: rbac.authorization.k8s.io",
                "Subject API group",
            ),
        ],
        p if p.ends_with("spec.volumes") => &[
            ("name", "name: ${1:config-vol}", "Volume name"),
            (
                "configMap",
                "configMap:\n  name: ${1:my-config}",
                "Populate volume from ConfigMap",
            ),
            (
                "secret",
                "secret:\n  secretName: ${1:my-secret}",
                "Populate volume from Secret",
            ),
            ("emptyDir", "emptyDir: {}", "Ephemeral scratch directory"),
            (
                "persistentVolumeClaim",
                "persistentVolumeClaim:\n  claimName: ${1:my-pvc}",
                "Mount PersistentVolumeClaim",
            ),
            (
                "hostPath",
                "hostPath:\n  path: ${1:/var/lib/data}",
                "Mount host node path",
            ),
            (
                "projected",
                "projected:\n  sources: []",
                "Combine multiple volume sources",
            ),
            (
                "downwardAPI",
                "downwardAPI:\n  items: []",
                "Expose Pod metadata as files",
            ),
            (
                "csi",
                "csi:\n  driver: ${1:csi.example.com}",
                "CSI ephemeral volume",
            ),
        ],
        p if p.ends_with("volumes.configMap") => &[
            ("name", "name: ${1:my-config}", "ConfigMap name"),
            (
                "defaultMode",
                "defaultMode: ${1:420}",
                "File permission mode",
            ),
            (
                "optional",
                "optional: ${1:false}",
                "Allow missing ConfigMap",
            ),
            (
                "items",
                "items:\n  - key: ${1:config.yaml}\n    path: ${2:config.yaml}",
                "Project specific keys to paths",
            ),
        ],
        p if p.ends_with("volumes.secret") => &[
            ("secretName", "secretName: ${1:my-secret}", "Secret name"),
            (
                "defaultMode",
                "defaultMode: ${1:420}",
                "File permission mode",
            ),
            ("optional", "optional: ${1:false}", "Allow missing Secret"),
            (
                "items",
                "items:\n  - key: ${1:tls.crt}\n    path: ${2:tls.crt}",
                "Project specific keys to paths",
            ),
        ],
        p if p.ends_with("volumes.persistentVolumeClaim") => &[
            (
                "claimName",
                "claimName: ${1:my-pvc}",
                "PersistentVolumeClaim name",
            ),
            ("readOnly", "readOnly: ${1:false}", "Mount read-only"),
        ],
        p if p.ends_with("spec.containers")
            || p.ends_with("spec.initContainers")
            || p.ends_with("spec.ephemeralContainers") =>
        {
            &[
                ("name", "name: ${1:app}", "Container name"),
                ("image", "image: ${1:nginx:1.27-alpine}", "Container image"),
                (
                    "imagePullPolicy",
                    "imagePullPolicy: ${1:IfNotPresent}",
                    "Image pull policy",
                ),
                (
                    "command",
                    "command: [\"${1:/bin/sh}\"]",
                    "Container entrypoint array",
                ),
                (
                    "args",
                    "args: [\"${1:-c}\", \"${2:echo ok}\"]",
                    "Container command arguments",
                ),
                (
                    "workingDir",
                    "workingDir: ${1:/app}",
                    "Container working directory",
                ),
                (
                    "ports",
                    "ports:\n  - name: ${1:http}\n    containerPort: ${2:8080}",
                    "Container ports",
                ),
                (
                    "env",
                    "env:\n  - name: ${1:ENV_VAR}\n    value: \"${2:value}\"",
                    "Environment variables",
                ),
                (
                    "envFrom",
                    "envFrom:\n  - configMapRef:\n      name: ${1:my-config}",
                    "Populate env from ConfigMap or Secret",
                ),
                (
                    "resources",
                    "resources:\n  requests:\n    cpu: ${1:100m}\n    memory: ${2:128Mi}\n  limits:\n    cpu: ${3:500m}\n    memory: ${4:256Mi}",
                    "CPU and memory requests/limits",
                ),
                (
                    "livenessProbe",
                    "livenessProbe:\n  httpGet:\n    path: ${1:/healthz}\n    port: ${2:http}\n  initialDelaySeconds: ${3:15}\n  periodSeconds: ${4:20}",
                    "Container liveness probe",
                ),
                (
                    "readinessProbe",
                    "readinessProbe:\n  httpGet:\n    path: ${1:/readyz}\n    port: ${2:http}\n  initialDelaySeconds: ${3:5}\n  periodSeconds: ${4:10}",
                    "Container readiness probe",
                ),
                (
                    "startupProbe",
                    "startupProbe:\n  httpGet:\n    path: ${1:/healthz}\n    port: ${2:http}\n  failureThreshold: ${3:30}\n  periodSeconds: ${4:10}",
                    "Container startup probe",
                ),
                (
                    "volumeMounts",
                    "volumeMounts:\n  - name: ${1:config-vol}\n    mountPath: ${2:/etc/config}",
                    "Container volume mounts",
                ),
                (
                    "lifecycle",
                    "lifecycle:\n  preStop:\n    exec:\n      command: [\"/bin/sh\", \"-c\", \"sleep 5\"]",
                    "Container lifecycle hooks",
                ),
                (
                    "securityContext",
                    "securityContext:\n  runAsNonRoot: true\n  allowPrivilegeEscalation: false",
                    "Container security context",
                ),
            ]
        }
        "spec.ports" if kind == Some("Service") => &[
            ("name", "name: ${1:http}", "Service port name"),
            ("port", "port: ${1:80}", "Service port number"),
            (
                "targetPort",
                "targetPort: ${1:http}",
                "Target Pod port name or number",
            ),
            ("protocol", "protocol: ${1:TCP}", "TCP, UDP, or SCTP"),
            (
                "nodePort",
                "nodePort: ${1:30080}",
                "Static NodePort (30000-32767)",
            ),
        ],
        p if p.ends_with(".ports") => &[
            ("name", "name: ${1:http}", "Port name"),
            (
                "containerPort",
                "containerPort: ${1:8080}",
                "Port number exposed on Pod IP",
            ),
            ("protocol", "protocol: ${1:TCP}", "TCP, UDP, or SCTP"),
            (
                "hostPort",
                "hostPort: ${1:8080}",
                "Port exposed on host node",
            ),
        ],
        p if p.ends_with(".env") => &[
            ("name", "name: ${1:ENV_VAR}", "Environment variable name"),
            ("value", "value: \"${1:value}\"", "Literal string value"),
            (
                "valueFrom",
                "valueFrom:\n  secretKeyRef:\n    name: ${1:my-secret}\n    key: ${2:KEY}",
                "Source from ConfigMap, Secret, or Pod field",
            ),
        ],
        p if p.ends_with(".env.valueFrom") => &[
            (
                "configMapKeyRef",
                "configMapKeyRef:\n  name: ${1:my-config}\n  key: ${2:KEY}",
                "Select key from ConfigMap",
            ),
            (
                "secretKeyRef",
                "secretKeyRef:\n  name: ${1:my-secret}\n  key: ${2:KEY}",
                "Select key from Secret",
            ),
            (
                "fieldRef",
                "fieldRef:\n  fieldPath: ${1:metadata.namespace}",
                "Select Pod field",
            ),
            (
                "resourceFieldRef",
                "resourceFieldRef:\n  resource: ${1:limits.memory}",
                "Select container resource limit/request",
            ),
        ],
        p if p.ends_with(".envFrom") => &[
            (
                "configMapRef",
                "configMapRef:\n  name: ${1:my-config}",
                "Import all keys from ConfigMap",
            ),
            (
                "secretRef",
                "secretRef:\n  name: ${1:my-secret}",
                "Import all keys from Secret",
            ),
            (
                "prefix",
                "prefix: ${1:APP_}",
                "Prefix prepended to imported variable names",
            ),
        ],
        p if p.ends_with(".volumeMounts") => &[
            ("name", "name: ${1:config-vol}", "Pod volume name to mount"),
            (
                "mountPath",
                "mountPath: ${1:/etc/config}",
                "Path inside container filesystem",
            ),
            (
                "subPath",
                "subPath: ${1:config.yaml}",
                "Sub-path within referenced volume",
            ),
            ("readOnly", "readOnly: ${1:true}", "Mount volume read-only"),
        ],
        p if p.ends_with(".resources") => &[
            (
                "requests",
                "requests:\n  cpu: ${1:100m}\n  memory: ${2:128Mi}",
                "Minimum guaranteed compute resources",
            ),
            (
                "limits",
                "limits:\n  cpu: ${1:500m}\n  memory: ${2:256Mi}",
                "Maximum allowed compute resources",
            ),
        ],
        p if p.ends_with(".resources.requests") || p.ends_with(".resources.limits") => &[
            ("cpu", "cpu: ${1:100m}", "CPU cores or millicores"),
            ("memory", "memory: ${1:128Mi}", "Memory bytes (Mi, Gi)"),
            (
                "ephemeral-storage",
                "ephemeral-storage: ${1:1Gi}",
                "Ephemeral local storage",
            ),
        ],
        p if p.ends_with(".livenessProbe")
            || p.ends_with(".readinessProbe")
            || p.ends_with(".startupProbe") =>
        {
            &[
                (
                    "httpGet",
                    "httpGet:\n  path: ${1:/healthz}\n  port: ${2:http}",
                    "HTTP GET health check",
                ),
                (
                    "tcpSocket",
                    "tcpSocket:\n  port: ${1:8080}",
                    "TCP socket health check",
                ),
                ("grpc", "grpc:\n  port: ${1:50051}", "gRPC health check"),
                (
                    "exec",
                    "exec:\n  command: [\"${1:cat}\", \"${2:/tmp/healthy}\"]",
                    "Command execution check",
                ),
                (
                    "initialDelaySeconds",
                    "initialDelaySeconds: ${1:5}",
                    "Delay before first probe",
                ),
                (
                    "periodSeconds",
                    "periodSeconds: ${1:10}",
                    "Probe interval in seconds",
                ),
                (
                    "timeoutSeconds",
                    "timeoutSeconds: ${1:1}",
                    "Probe timeout in seconds",
                ),
                (
                    "failureThreshold",
                    "failureThreshold: ${1:3}",
                    "Consecutive failures before action",
                ),
                (
                    "successThreshold",
                    "successThreshold: ${1:1}",
                    "Consecutive successes required",
                ),
            ]
        }
        p if p.ends_with(".securityContext") => &[
            (
                "runAsNonRoot",
                "runAsNonRoot: ${1:true}",
                "Require non-root UID",
            ),
            (
                "runAsUser",
                "runAsUser: ${1:1000}",
                "UID to run container entrypoint",
            ),
            (
                "runAsGroup",
                "runAsGroup: ${1:1000}",
                "Primary GID for container",
            ),
            (
                "fsGroup",
                "fsGroup: ${1:2000}",
                "Supplemental group for mounted volumes",
            ),
            (
                "allowPrivilegeEscalation",
                "allowPrivilegeEscalation: ${1:false}",
                "Disallow gaining more privileges",
            ),
            (
                "privileged",
                "privileged: ${1:false}",
                "Run container as privileged",
            ),
            (
                "readOnlyRootFilesystem",
                "readOnlyRootFilesystem: ${1:true}",
                "Mount root filesystem read-only",
            ),
            (
                "capabilities",
                "capabilities:\n  drop: [\"ALL\"]",
                "Linux capabilities add/drop",
            ),
            (
                "seccompProfile",
                "seccompProfile:\n  type: RuntimeDefault",
                "Seccomp syscall filter",
            ),
        ],
        _ => &[],
    }
}

fn live_templates() -> &'static [(&'static str, &'static str, &'static str)] {
    &[
        (
            "kdep",
            "Kubernetes Deployment",
            "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: ${1:my-app}\n  labels:\n    app.kubernetes.io/name: ${1:my-app}\nspec:\n  replicas: ${2:2}\n  selector:\n    matchLabels:\n      app.kubernetes.io/name: ${1:my-app}\n  template:\n    metadata:\n      labels:\n        app.kubernetes.io/name: ${1:my-app}\n    spec:\n      containers:\n        - name: ${1:my-app}\n          image: ${3:nginx:1.27-alpine}\n          ports:\n            - name: http\n              containerPort: ${4:80}\n          resources:\n            requests:\n              cpu: 100m\n              memory: 128Mi\n            limits:\n              cpu: 500m\n              memory: 256Mi\n$0",
        ),
        (
            "ksvc",
            "Kubernetes Service",
            "apiVersion: v1\nkind: Service\nmetadata:\n  name: ${1:my-app}\nspec:\n  type: ${2:ClusterIP}\n  selector:\n    app.kubernetes.io/name: ${1:my-app}\n  ports:\n    - name: http\n      port: ${3:80}\n      targetPort: ${4:http}\n$0",
        ),
        (
            "kcm",
            "Kubernetes ConfigMap",
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: ${1:my-config}\ndata:\n  ${2:APP_ENV}: \"${3:production}\"\n$0",
        ),
        (
            "ksec",
            "Kubernetes Secret",
            "apiVersion: v1\nkind: Secret\nmetadata:\n  name: ${1:my-secret}\ntype: Opaque\nstringData:\n  ${2:API_KEY}: \"${3:change-me}\"\n$0",
        ),
        (
            "king",
            "Kubernetes Ingress",
            "apiVersion: networking.k8s.io/v1\nkind: Ingress\nmetadata:\n  name: ${1:my-ingress}\nspec:\n  ingressClassName: ${2:nginx}\n  rules:\n    - host: ${3:app.example.com}\n      http:\n        paths:\n          - path: /\n            pathType: Prefix\n            backend:\n              service:\n                name: ${4:my-app}\n                port:\n                  name: ${5:http}\n$0",
        ),
        (
            "kpod",
            "Kubernetes Pod",
            "apiVersion: v1\nkind: Pod\nmetadata:\n  name: ${1:my-pod}\n  labels:\n    app.kubernetes.io/name: ${1:my-pod}\nspec:\n  containers:\n    - name: ${1:my-pod}\n      image: ${2:nginx:1.27-alpine}\n      resources:\n        requests:\n          cpu: 100m\n          memory: 128Mi\n        limits:\n          cpu: 500m\n          memory: 256Mi\n$0",
        ),
        (
            "kust",
            "Kustomization",
            "apiVersion: kustomize.config.k8s.io/v1beta1\nkind: Kustomization\nnamespace: ${1:default}\nresources:\n  - ${2:deployment.yaml}\n  - ${3:service.yaml}\n$0",
        ),
    ]
}
