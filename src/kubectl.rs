//! Wrapper around `kubectl`, `helm`, and `kustomize` CLI execution,
//! cluster state parsing, and interactive terminal session launching.

use openidea_plugin::host::{self, storage};
use openidea_plugin::{Error, HttpRequest, Invocation, ProcessOutput, Project};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufReader, Read};

pub const DEFAULT_DEBUGGER_IMAGE: &str = "dengliu/k8sdebugger:3cfd912";

/// Active Kubernetes connection configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KubeConfigState {
    pub kubeconfig: String,
    pub context: String,
    pub namespace: String,
    pub all_namespaces: bool,
    #[serde(default)]
    pub selected_namespaces: Vec<String>,
    pub tail_lines: u32,
    pub shell: String,
    pub debugger_image: String,
}

pub fn normalize_namespace_list<I, S>(items: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out: Vec<String> = Vec::new();
    for raw in items {
        let trimmed = raw.as_ref().trim();
        if !trimmed.is_empty() && !out.iter().any(|existing| existing == trimmed) {
            out.push(trimmed.to_owned());
        }
    }
    out
}

impl KubeConfigState {
    pub fn load() -> Self {
        let kubeconfig = host::setting_string("kubeconfig").unwrap_or_default();
        let context = storage::get("context")
            .ok()
            .flatten()
            .filter(|s| !s.is_empty())
            .or_else(|| host::setting_string("context").filter(|s| !s.is_empty()))
            .unwrap_or_default();
        let fallback_ns = storage::get("namespace")
            .ok()
            .flatten()
            .filter(|s| !s.is_empty())
            .or_else(|| host::setting_string("namespace").filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "default".into());
        let stored_multi = storage::get("selectedNamespaces")
            .ok()
            .flatten()
            .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
            .map(normalize_namespace_list)
            .unwrap_or_default();
        let selected_namespaces = if stored_multi.is_empty() {
            normalize_namespace_list([fallback_ns.as_str()])
        } else {
            stored_multi
        };
        let namespace = selected_namespaces
            .first()
            .cloned()
            .unwrap_or_else(|| "default".into());
        let all_namespaces = storage::get("allNamespaces")
            .ok()
            .flatten()
            .map(|v| v == "true")
            .unwrap_or_else(|| host::setting_bool("allNamespaces").unwrap_or(false));
        let tail_lines = host::setting_number("tailLines")
            .map(|n| (n as u32).clamp(10, 10_000))
            .unwrap_or(200);
        let shell = host::setting_string("shell")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/sh".into());
        let debugger_image = host::setting_string("debuggerImage")
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_DEBUGGER_IMAGE.into());

        Self {
            kubeconfig,
            context,
            namespace,
            all_namespaces,
            selected_namespaces,
            tail_lines,
            shell,
            debugger_image,
        }
    }

    /// Non-empty list of selected namespaces when `!self.all_namespaces`, or empty when `all_namespaces` is active.
    pub fn active_namespaces(&self) -> Vec<String> {
        if self.all_namespaces {
            return Vec::new();
        }
        let normalized = normalize_namespace_list(&self.selected_namespaces);
        if !normalized.is_empty() {
            normalized
        } else if !self.namespace.trim().is_empty() {
            vec![self.namespace.trim().to_owned()]
        } else {
            vec!["default".into()]
        }
    }

    pub fn is_multi_namespace(&self) -> bool {
        !self.all_namespaces && self.active_namespaces().len() > 1
    }

    pub fn with_single_namespace(&self, ns: &str) -> Self {
        let clean = ns.trim();
        let target = if clean.is_empty() { "default" } else { clean };
        let mut copy = self.clone();
        copy.all_namespaces = false;
        copy.namespace = target.to_owned();
        copy.selected_namespaces = vec![target.to_owned()];
        copy
    }

    pub fn set_namespaces<I, S>(&mut self, items: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let normalized = normalize_namespace_list(items);
        if normalized.is_empty() {
            self.all_namespaces = true;
            if self.selected_namespaces.is_empty() {
                let primary = if self.namespace.trim().is_empty() {
                    "default".to_owned()
                } else {
                    self.namespace.trim().to_owned()
                };
                self.selected_namespaces = vec![primary.clone()];
                self.namespace = primary;
            }
        } else {
            self.all_namespaces = false;
            self.namespace = normalized[0].clone();
            self.selected_namespaces = normalized;
        }
    }

    pub fn persist_scope(&self) {
        let primary = if self.namespace.trim().is_empty() {
            "default"
        } else {
            self.namespace.trim()
        };
        let list = {
            let norm = normalize_namespace_list(&self.selected_namespaces);
            if norm.is_empty() {
                vec![primary.to_owned()]
            } else {
                norm
            }
        };
        let _ = storage::set("namespace", primary);
        let _ = storage::set(
            "allNamespaces",
            if self.all_namespaces { "true" } else { "false" },
        );
        if let Ok(json) = serde_json::to_string(&list) {
            let _ = storage::set("selectedNamespaces", &json);
        }
    }

    pub fn base_args(&self, include_namespace: bool) -> Vec<String> {
        let mut args = Vec::new();
        if !self.kubeconfig.trim().is_empty() {
            args.push(format!("--kubeconfig={}", self.kubeconfig.trim()));
        }
        if !self.context.trim().is_empty() {
            args.push(format!("--context={}", self.context.trim()));
        }
        if include_namespace && !self.all_namespaces && !self.namespace.trim().is_empty() {
            args.push("-n".into());
            args.push(self.namespace.trim().to_owned());
        }
        args
    }

    pub fn push_scope_flags(&self, args: &mut Vec<String>) {
        if self.all_namespaces {
            args.push("--all-namespaces".into());
        }
    }

    pub fn target_args(&self, namespace_override: Option<&str>) -> Vec<String> {
        let mut args = Vec::new();
        if !self.kubeconfig.trim().is_empty() {
            args.push(format!("--kubeconfig={}", self.kubeconfig.trim()));
        }
        if !self.context.trim().is_empty() {
            args.push(format!("--context={}", self.context.trim()));
        }
        let ns = namespace_override
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| self.namespace.trim());
        if !ns.is_empty() {
            args.push("-n".into());
            args.push(ns.to_owned());
        }
        args
    }
}

/// Summary of one cluster context from kubeconfig (`~/.kube/config`), enriched with Freelens store metadata when present.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterContextInfo {
    pub name: String,
    pub cluster: String,
    pub server: String,
    pub user: String,
    pub namespace: String,
    pub source: String,
    pub short_name: String,
    pub version: String,
    pub distro: String,
    pub nodes: Option<u32>,
    pub last_seen: String,
    pub hotbar_pinned: bool,
    pub hotbar_order: Option<u32>,
}

/// State of one container, initContainer, sidecar, or ephemeralContainer in a Pod (rendered as colored status squares in Freelens).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerStateBadge {
    pub name: String,
    /// `"running"` | `"terminated"` | `"waiting"`
    pub state: String,
    pub ready: bool,
    pub reason: String,
    pub is_init: bool,
    #[serde(default)]
    pub is_ephemeral: bool,
    #[serde(default)]
    pub is_sidecar: bool,
    #[serde(default)]
    pub started_at: String,
    #[serde(default)]
    pub finished_at: String,
    #[serde(default)]
    pub exit_code: Option<i64>,
    #[serde(default)]
    pub container_id: String,
}

/// Summary of one Kubernetes resource returned by `kubectl get`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterResource {
    pub kind: String,
    pub name: String,
    pub namespace: String,
    pub status: String,
    pub ready: String,
    pub age: String,
    pub detail: String,
    pub containers: Vec<String>,
    pub ports: Vec<String>,
    /// Data entry keys when `kind` is `ConfigMap` or `Secret` (for editor auto-completion).
    pub data_keys: Vec<String>,
    #[serde(default)]
    pub container_states: Vec<ContainerStateBadge>,
    #[serde(default)]
    pub cpu: String,
    #[serde(default)]
    pub memory: String,
    #[serde(default)]
    pub restarts: u64,
    #[serde(default)]
    pub controlled_by_kind: String,
    #[serde(default)]
    pub controlled_by_name: String,
    #[serde(default)]
    pub pod_ip: String,
    #[serde(default)]
    pub host_ip: String,
    #[serde(default)]
    pub node_name: String,
    #[serde(default)]
    pub qos: String,
    #[serde(default)]
    pub labels: Vec<String>,
}

/// Summary of one Kubernetes event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterEvent {
    pub kind: String,
    pub object: String,
    pub namespace: String,
    pub reason: String,
    pub message: String,
    pub event_type: String,
    pub count: u64,
    pub last_seen: String,
}

/// Summary of one Helm release.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelmRelease {
    pub name: String,
    pub namespace: String,
    pub revision: String,
    pub status: String,
    pub chart: String,
    pub app_version: String,
    pub updated: String,
}

pub const DEFAULT_PAGE_LIMIT: u32 = 200;
pub const MAX_APPENDED_ITEMS: usize = 3_000;
/// Maximum total items in a category scope where 100% of items are loaded up front and live `watch=true`
/// delta streaming is enabled (matching Freelens's full-store + live watch experience for scopes <= 500 items).
/// Scopes larger than this keep using bounded `limit` + `continue` auto-scroll streaming.
pub const WATCH_ELIGIBLE_MAX_ITEMS: u64 = 500;
/// Maximum number of concurrent per-namespace `watch=true` streams opened when multiple namespaces
/// are selected (keeps total `Child` handles <= 6 alongside an optional Drawer watch, within OpenIDEA's 8-child cap).
pub const MAX_MULTI_NAMESPACE_WATCHES: usize = 5;

/// Pagination & watch metadata for a single resource category returned by Kubernetes API `limit` + `continue`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategoryPageInfo {
    pub category: String,
    pub limit: u32,
    #[serde(default)]
    pub request_token: String,
    pub continue_token: String,
    pub remaining_count: Option<u64>,
    pub loaded_count: u64,
    pub page_index: u32,
    pub mode: String,
    #[serde(default)]
    pub resource_version: String,
    #[serde(default)]
    pub namespace_versions: BTreeMap<String, String>,
    #[serde(default)]
    pub watch_supported: bool,
}

/// Snapshot of the cluster state displayed in the Kubernetes tool window.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterSnapshot {
    pub connected: bool,
    pub error: Option<String>,
    pub current_context: String,
    pub contexts: Vec<String>,
    pub cluster_catalog: Vec<ClusterContextInfo>,
    pub current_namespace: String,
    pub all_namespaces: bool,
    #[serde(default)]
    pub selected_namespaces: Vec<String>,
    pub namespaces: Vec<String>,
    pub resources: Vec<ClusterResource>,
    pub events: Vec<ClusterEvent>,
    pub helm_releases: Vec<HelmRelease>,
    #[serde(default)]
    pub page_info: BTreeMap<String, CategoryPageInfo>,
    #[serde(default)]
    pub overview_counts: BTreeMap<String, u64>,
}

/// Finds the active `Project` from `ctx` or the first open project in the workspace.
pub fn resolve_project(ctx: &Invocation) -> Result<Project, Error> {
    if let Some(p) = ctx.project() {
        return Ok(p);
    }
    host::projects()?
        .into_iter()
        .next()
        .ok_or_else(|| Error::Invalid("open a project to run Kubernetes commands".into()))
}

#[derive(Clone, Debug)]
pub struct FetchOptions {
    pub is_background_poll: bool,
    pub include_metrics: bool,
    pub limit: u32,
    pub continue_token: String,
    pub append: bool,
    pub page_index: u32,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            is_background_poll: false,
            include_metrics: true,
            limit: DEFAULT_PAGE_LIMIT,
            continue_token: String::new(),
            append: false,
            page_index: 1,
        }
    }
}

fn should_attach_request_timeout(args: &[String]) -> bool {
    if args.iter().any(|a| {
        a.starts_with("--request-timeout")
            || a.starts_with("--timeout")
            || a == "config"
            || a == "kustomize"
            || a == "--dry-run=client"
    }) {
        return false;
    }
    !args.iter().any(|a| a == "exec" || a == "port-forward")
}

/// Runs `kubectl` with an explicit `--request-timeout` when `timeout_secs` is `Some`.
pub fn run_kubectl_timed(
    project: &Project,
    args: &[String],
    stdin: Option<&str>,
    timeout_secs: Option<u32>,
) -> Result<ProcessOutput, Error> {
    if let Some(secs) = timeout_secs
        && should_attach_request_timeout(args)
    {
        let mut timed = Vec::with_capacity(args.len() + 1);
        timed.extend_from_slice(args);
        timed.push(format!("--request-timeout={secs}s"));
        return project.run_process("kubectl", &timed, stdin);
    }
    project.run_process("kubectl", args, stdin)
}

/// Runs `kubectl` with `args` in `project` (defaulting to a 15s request timeout for cluster calls).
pub fn run_kubectl(
    project: &Project,
    args: &[String],
    stdin: Option<&str>,
) -> Result<ProcessOutput, Error> {
    run_kubectl_timed(project, args, stdin, Some(15))
}

pub type KubeconfigOverview = (String, Vec<String>, Option<String>, Vec<ClusterContextInfo>);

/// Generates a 2–3 letter uppercase badge (like Freelens `AP`, `FDC`, `FPG`) from a context name.
pub fn compute_short_name(name: &str) -> String {
    let parts: Vec<&str> = name
        .split(['-', '_', '.', '/'])
        .filter(|s| !s.is_empty())
        .collect();
    if parts.len() >= 3 {
        parts
            .iter()
            .take(3)
            .filter_map(|p| p.chars().next())
            .map(|c| c.to_ascii_uppercase())
            .collect()
    } else if parts.len() == 2 {
        parts
            .iter()
            .filter_map(|p| p.chars().next())
            .map(|c| c.to_ascii_uppercase())
            .collect()
    } else {
        name.chars()
            .take(3)
            .map(|c| c.to_ascii_uppercase())
            .collect()
    }
}

/// Infers the user's home directory from `project.root()` (e.g. `/Users/<user>/...` or `/home/<user>/...`).
fn infer_home_dir(project: &Project) -> Option<String> {
    let root = project.root().ok()?.replace('\\', "/");
    let segs: Vec<&str> = root.split('/').filter(|s| !s.is_empty()).collect();
    if segs.len() >= 2 && matches!(segs[0], "Users" | "home") {
        return Some(format!("/{}/{}", segs[0], segs[1]));
    }
    if root.len() >= 9
        && root.as_bytes().get(1) == Some(&b':')
        && segs.len() >= 3
        && segs[1].eq_ignore_ascii_case("Users")
    {
        return Some(format!("{}/{}/{}", segs[0], segs[1], segs[2]));
    }
    None
}

#[derive(Deserialize)]
struct RawFreelensConfigMap {
    #[serde(default)]
    data: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct RawFreelensClusterStore {
    #[serde(default)]
    clusters: Vec<RawFreelensCluster>,
}

#[derive(Deserialize)]
struct RawFreelensCluster {
    #[serde(default)]
    id: String,
    #[serde(default, rename = "contextName")]
    context_name: String,
    #[serde(default, rename = "kubeConfigPath")]
    kube_config_path: String,
    #[serde(default)]
    metadata: Option<RawFreelensClusterMeta>,
}

#[derive(Deserialize)]
struct RawFreelensClusterMeta {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    distribution: Option<String>,
    #[serde(default)]
    nodes: Option<u32>,
    #[serde(default, rename = "lastSeen")]
    last_seen: Option<String>,
}

#[derive(Deserialize)]
struct RawFreelensHotbarStore {
    #[serde(default)]
    hotbars: Vec<RawFreelensHotbar>,
}

#[derive(Deserialize)]
struct RawFreelensHotbar {
    #[serde(default)]
    items: Vec<Option<RawFreelensHotbarItem>>,
}

#[derive(Deserialize)]
struct RawFreelensHotbarItem {
    #[serde(default)]
    entity: Option<RawFreelensHotbarEntity>,
}

#[derive(Deserialize)]
struct RawFreelensHotbarEntity {
    #[serde(default)]
    uid: String,
    #[serde(default)]
    name: String,
    #[serde(default, rename = "shortName")]
    short_name: Option<String>,
}

struct FreelensClusterEntry {
    id: String,
    version: String,
    nodes: Option<u32>,
    distro: String,
    last_seen: String,
    kube_config_path: String,
}

/// Enriches `catalog` with Freelens `lens-cluster-store.json` and `lens-hotbar-store.json` when installed locally.
pub fn merge_freelens_stores(
    catalog: &mut [ClusterContextInfo],
    clusters_json: Option<&str>,
    hotbars_json: Option<&str>,
) {
    let mut by_context: BTreeMap<String, FreelensClusterEntry> = BTreeMap::new();
    if let Some(cjson) = clusters_json
        && let Ok(store) = serde_json::from_str::<RawFreelensClusterStore>(cjson)
    {
        for cl in store.clusters {
            if cl.context_name.is_empty() {
                continue;
            }
            let meta = cl.metadata.unwrap_or(RawFreelensClusterMeta {
                version: None,
                distribution: None,
                nodes: None,
                last_seen: None,
            });
            let ver = meta.version.unwrap_or_default();
            let dist = meta
                .distribution
                .filter(|d| d != "unknown")
                .unwrap_or_default();
            let last_seen = meta.last_seen.unwrap_or_default();
            by_context.insert(
                cl.context_name,
                FreelensClusterEntry {
                    id: cl.id,
                    version: ver,
                    nodes: meta.nodes,
                    distro: dist,
                    last_seen,
                    kube_config_path: cl.kube_config_path,
                },
            );
        }
    }

    let mut hotbar_by_name_or_uid: BTreeMap<String, (String, u32)> = BTreeMap::new();
    if let Some(hjson) = hotbars_json
        && let Ok(hstore) = serde_json::from_str::<RawFreelensHotbarStore>(hjson)
    {
        let mut order_idx = 0u32;
        for hb in hstore.hotbars {
            for item in hb.items.into_iter().flatten() {
                if let Some(ent) = item.entity {
                    if ent.uid == "welcome-page-entity" || ent.uid == "catalog-entity" {
                        continue;
                    }
                    let badge = ent
                        .short_name
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_ascii_uppercase())
                        .unwrap_or_else(|| compute_short_name(&ent.name));
                    let ord = order_idx;
                    order_idx += 1;
                    if !ent.name.is_empty() {
                        hotbar_by_name_or_uid.insert(ent.name, (badge.clone(), ord));
                    }
                    if !ent.uid.is_empty() {
                        hotbar_by_name_or_uid.insert(ent.uid, (badge, ord));
                    }
                }
            }
        }
    }

    let mut used_hotbar_badges: BTreeMap<String, usize> = BTreeMap::new();
    for entry in catalog.iter_mut() {
        let mut cluster_uid = String::new();
        if let Some(fl) = by_context.get(&entry.name) {
            cluster_uid = fl.id.clone();
            if !fl.version.is_empty() {
                entry.version = fl.version.clone();
            }
            if fl.nodes.is_some() {
                entry.nodes = fl.nodes;
            }
            if !fl.distro.is_empty() {
                entry.distro = fl.distro.clone();
            }
            if !fl.last_seen.is_empty() {
                entry.last_seen = fl.last_seen.clone();
            }
            if !fl.kube_config_path.is_empty()
                && entry.source == "~/.kube/config"
                && fl.kube_config_path.ends_with("/.kube/config")
            {
                entry.source = "~/.kube/config".into();
            }
        }
        if let Some((badge, ord)) = hotbar_by_name_or_uid.get(&entry.name).or_else(|| {
            (!cluster_uid.is_empty())
                .then(|| hotbar_by_name_or_uid.get(&cluster_uid))
                .flatten()
        }) {
            entry.hotbar_pinned = true;
            entry.hotbar_order = Some(*ord);
            let count = used_hotbar_badges.entry(badge.clone()).or_insert(0);
            *count += 1;
            if *count > 1 && entry.name.ends_with("-staging") {
                entry.short_name = format!("{badge}S");
            } else {
                entry.short_name = badge.clone();
            }
        }
        if entry.short_name.is_empty() {
            entry.short_name = compute_short_name(&entry.name);
        }
    }
}

/// Reads Freelens's `lens-cluster-store.json` and `lens-hotbar-store.json` via a single offline
/// `kubectl create configmap --dry-run=client -o json` call (requires no extra permissions beyond `process:kubectl`).
fn load_local_freelens_metadata(project: &Project, catalog: &mut [ClusterContextInfo]) {
    let Some(home) = infer_home_dir(project) else {
        return;
    };
    let candidate_dirs = [
        format!("{home}/Library/Application Support/Freelens"),
        format!("{home}/.config/Freelens"),
        format!("{home}/AppData/Roaming/Freelens"),
    ];
    for dir in candidate_dirs {
        let clusters_file = format!("{dir}/lens-cluster-store.json");
        let hotbars_file = format!("{dir}/lens-hotbar-store.json");
        let args = vec![
            "create".to_owned(),
            "configmap".into(),
            "freelens-sync".into(),
            format!("--from-file=clusters={clusters_file}"),
            format!("--from-file=hotbars={hotbars_file}"),
            "--dry-run=client".into(),
            "-o".into(),
            "json".into(),
        ];
        if let Ok(out) = run_kubectl(project, &args, None)
            && out.status == Some(0)
            && let Ok(cm) = serde_json::from_str::<RawFreelensConfigMap>(&out.stdout)
        {
            merge_freelens_stores(
                catalog,
                cm.data.get("clusters").map(String::as_str),
                cm.data.get("hotbars").map(String::as_str),
            );
            return;
        }
    }
}

/// Queries `kubectl config view -o json` to get available contexts, active context, active namespace, and full cluster catalog.
pub fn fetch_kubeconfig_overview(
    project: &Project,
    cfg: &KubeConfigState,
) -> Result<KubeconfigOverview, Error> {
    let mut args = Vec::new();
    if !cfg.kubeconfig.trim().is_empty() {
        args.push(format!("--kubeconfig={}", cfg.kubeconfig.trim()));
    }
    args.extend(["config".into(), "view".into(), "-o".into(), "json".into()]);
    let out = run_kubectl(project, &args, None)?;
    if out.status != Some(0) {
        return Err(Error::Failed(first_line_or(
            &out.stderr,
            "kubectl config view failed",
        )));
    }
    let source = if cfg.kubeconfig.trim().is_empty() {
        "~/.kube/config".to_owned()
    } else {
        cfg.kubeconfig.trim().to_owned()
    };
    let (current, contexts, active_ns, mut catalog) =
        parse_kubeconfig_catalog(&out.stdout, &cfg.context, &source);
    load_local_freelens_metadata(project, &mut catalog);
    Ok((current, contexts, active_ns, catalog))
}

#[derive(Deserialize)]
struct RawKubeconfig {
    #[serde(rename = "current-context", default)]
    current_context: String,
    #[serde(default)]
    clusters: Vec<RawClusterEntry>,
    #[serde(default)]
    contexts: Vec<RawContextEntry>,
}

#[derive(Deserialize)]
struct RawClusterEntry {
    #[serde(default)]
    name: String,
    #[serde(default)]
    cluster: Option<RawClusterDetail>,
}

#[derive(Deserialize)]
struct RawClusterDetail {
    #[serde(default)]
    server: Option<String>,
}

#[derive(Deserialize)]
struct RawContextEntry {
    #[serde(default)]
    name: String,
    #[serde(default)]
    context: Option<RawContextDetail>,
}

#[derive(Deserialize)]
struct RawContextDetail {
    #[serde(default)]
    cluster: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    namespace: Option<String>,
}

/// Parses `kubectl config view -o json` output using typed deserialization (skipping cert blobs).
/// When `current-context` is not set in kubeconfig, falls back to the first available context.
pub fn parse_kubeconfig_json(
    json_str: &str,
    override_context: &str,
) -> (String, Vec<String>, Option<String>) {
    let (cur, ctxs, ns, _) = parse_kubeconfig_catalog(json_str, override_context, "~/.kube/config");
    (cur, ctxs, ns)
}

pub fn parse_kubeconfig_catalog(
    json_str: &str,
    override_context: &str,
    source: &str,
) -> KubeconfigOverview {
    let Ok(raw) = serde_json::from_str::<RawKubeconfig>(json_str) else {
        return (override_context.to_owned(), Vec::new(), None, Vec::new());
    };
    let mut cluster_servers = BTreeMap::new();
    for cl in raw.clusters {
        if !cl.name.is_empty()
            && let Some(server) = cl.cluster.and_then(|d| d.server)
        {
            cluster_servers.insert(cl.name, server);
        }
    }
    let mut contexts = Vec::with_capacity(raw.contexts.len());
    let mut catalog = Vec::with_capacity(raw.contexts.len());
    let mut context_namespaces = BTreeMap::new();
    for item in raw.contexts {
        if !item.name.is_empty() {
            let (cluster_name, user_name, ns_opt) = match item.context {
                Some(c) => (
                    c.cluster.unwrap_or_else(|| item.name.clone()),
                    c.user.unwrap_or_default(),
                    c.namespace.filter(|s| !s.is_empty()),
                ),
                None => (item.name.clone(), String::new(), None),
            };
            if let Some(ref ns) = ns_opt {
                context_namespaces.insert(item.name.clone(), ns.clone());
            }
            let server = cluster_servers
                .get(&cluster_name)
                .cloned()
                .unwrap_or_default();
            let distro = if server.contains(".azmk8s.io") {
                "aks".into()
            } else if server.contains(".eks.amazonaws.com") {
                "eks".into()
            } else if server.contains("gke") {
                "gke".into()
            } else if item.name.contains("docker-desktop") {
                "docker-desktop".into()
            } else if item.name.contains("minikube") {
                "minikube".into()
            } else if item.name.starts_with("kind-") {
                "kind".into()
            } else {
                String::new()
            };
            let short_name = compute_short_name(&item.name);
            catalog.push(ClusterContextInfo {
                name: item.name.clone(),
                cluster: cluster_name,
                server,
                user: user_name,
                namespace: ns_opt.unwrap_or_else(|| "default".into()),
                source: source.to_owned(),
                short_name,
                version: String::new(),
                distro,
                nodes: None,
                last_seen: String::new(),
                hotbar_pinned: false,
                hotbar_order: None,
            });
            contexts.push(item.name);
        }
    }
    let current = if !override_context.trim().is_empty() {
        override_context.trim().to_owned()
    } else if !raw.current_context.trim().is_empty() {
        raw.current_context.trim().to_owned()
    } else {
        contexts.first().cloned().unwrap_or_default()
    };
    let active_ns = context_namespaces.remove(&current);
    (current, contexts, active_ns, catalog)
}

#[derive(Deserialize, Default)]
struct RawListMetadata {
    #[serde(default, rename = "resourceVersion")]
    resource_version: Option<String>,
    #[serde(default, rename = "continue")]
    continue_token: Option<String>,
    #[serde(default, rename = "remainingItemCount")]
    remaining_item_count: Option<u64>,
}

#[derive(Deserialize)]
struct RawResourceList {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    metadata: Option<RawListMetadata>,
    #[serde(default)]
    items: Option<Vec<RawResourceItem>>,
    #[serde(flatten)]
    single: Option<RawResourceItem>,
}

#[derive(Deserialize)]
struct RawResourceItem {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    metadata: Option<RawMetadata>,
    #[serde(default)]
    spec: Option<RawSpec>,
    #[serde(default)]
    status: Option<RawStatus>,
    #[serde(default, rename = "type")]
    secret_type: Option<String>,
    #[serde(default)]
    provisioner: Option<String>,
    #[serde(default, rename = "reclaimPolicy")]
    reclaim_policy: Option<String>,
    #[serde(default, rename = "volumeBindingMode")]
    volume_binding_mode: Option<String>,
    #[serde(default)]
    data: Option<BTreeMap<String, serde::de::IgnoredAny>>,
    #[serde(default, rename = "stringData")]
    string_data: Option<BTreeMap<String, serde::de::IgnoredAny>>,
    #[serde(default)]
    value: Option<i64>,
    #[serde(default, rename = "globalDefault")]
    global_default: Option<bool>,
    #[serde(default)]
    handler: Option<String>,
    #[serde(default, rename = "addressType")]
    address_type: Option<String>,
    #[serde(default)]
    webhooks: Option<Vec<serde::de::IgnoredAny>>,
    #[serde(default)]
    rules: Option<Vec<serde::de::IgnoredAny>>,
    #[serde(default)]
    secrets: Option<Vec<serde::de::IgnoredAny>>,
    #[serde(default, rename = "roleRef")]
    role_ref: Option<RawOwnerReference>,
    #[serde(default)]
    subjects: Option<Vec<RawOwnerReference>>,
}

#[derive(Deserialize)]
struct RawOwnerReference {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct RawMetadata {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    namespace: Option<String>,
    #[serde(default, rename = "resourceVersion")]
    resource_version: Option<String>,
    #[serde(default, rename = "creationTimestamp")]
    creation_timestamp: Option<String>,
    #[serde(default, rename = "ownerReferences")]
    owner_references: Option<Vec<RawOwnerReference>>,
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
struct RawSpec {
    #[serde(default, rename = "nodeName")]
    node_name: Option<String>,
    #[serde(default)]
    replicas: Option<u64>,
    #[serde(default)]
    completions: Option<u64>,
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default)]
    suspend: Option<bool>,
    #[serde(default, rename = "type")]
    service_type: Option<String>,
    #[serde(default, rename = "clusterIP")]
    cluster_ip: Option<String>,
    #[serde(default, rename = "ingressClassName")]
    ingress_class_name: Option<String>,
    #[serde(default)]
    rules: Option<Vec<RawIngressRule>>,
    #[serde(default, rename = "storageClassName")]
    storage_class_name: Option<String>,
    #[serde(default)]
    resources: Option<RawPvcResources>,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    names: Option<RawCrdNames>,
    #[serde(default)]
    containers: Option<Vec<RawContainer>>,
    #[serde(default, rename = "initContainers")]
    init_containers: Option<Vec<RawContainer>>,
    #[serde(default, rename = "ephemeralContainers")]
    ephemeral_containers: Option<Vec<RawContainer>>,
    #[serde(default)]
    template: Option<RawPodTemplate>,
    #[serde(default)]
    ports: Option<Vec<RawServicePort>>,
    #[serde(default, rename = "minReplicas")]
    min_replicas: Option<u64>,
    #[serde(default, rename = "maxReplicas")]
    max_replicas: Option<u64>,
    #[serde(default, rename = "scaleTargetRef")]
    scale_target_ref: Option<RawOwnerReference>,
    #[serde(default, rename = "minAvailable")]
    min_available: Option<Value>,
    #[serde(default, rename = "maxUnavailable")]
    max_unavailable: Option<Value>,
    #[serde(default, rename = "holderIdentity")]
    holder_identity: Option<String>,
    #[serde(default)]
    controller: Option<String>,
    #[serde(default)]
    capacity: Option<BTreeMap<String, String>>,
    #[serde(default, rename = "persistentVolumeReclaimPolicy")]
    pv_reclaim_policy: Option<String>,
    #[serde(default)]
    unschedulable: Option<bool>,
    #[serde(default)]
    taints: Option<Vec<RawTaint>>,
}

#[derive(Deserialize)]
struct RawTaint {
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    effect: Option<String>,
}

#[derive(Deserialize)]
struct RawPodTemplate {
    #[serde(default)]
    spec: Option<RawPodTemplateSpec>,
}

#[derive(Deserialize)]
struct RawPodTemplateSpec {
    #[serde(default)]
    containers: Option<Vec<RawContainer>>,
    #[serde(default, rename = "initContainers")]
    init_containers: Option<Vec<RawContainer>>,
}

#[derive(Deserialize)]
struct RawContainer {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "restartPolicy")]
    restart_policy: Option<String>,
    #[serde(default)]
    ports: Option<Vec<RawContainerPort>>,
}

#[derive(Deserialize)]
struct RawContainerPort {
    #[serde(default, rename = "containerPort")]
    container_port: Option<u64>,
}

#[derive(Deserialize)]
struct RawServicePort {
    #[serde(default)]
    port: Option<u64>,
    #[serde(default)]
    protocol: Option<String>,
}

#[derive(Deserialize)]
struct RawIngressRule {
    #[serde(default)]
    host: Option<String>,
}

#[derive(Deserialize)]
struct RawPvcResources {
    #[serde(default)]
    requests: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
struct RawCrdNames {
    #[serde(default)]
    kind: Option<String>,
}

#[derive(Deserialize)]
struct RawStatus {
    #[serde(default)]
    phase: Option<String>,
    #[serde(default, rename = "podIP")]
    pod_ip: Option<String>,
    #[serde(default, rename = "hostIP")]
    host_ip: Option<String>,
    #[serde(default, rename = "qosClass")]
    qos_class: Option<String>,
    #[serde(default, rename = "containerStatuses")]
    container_statuses: Option<Vec<RawContainerStatus>>,
    #[serde(default, rename = "initContainerStatuses")]
    init_container_statuses: Option<Vec<RawContainerStatus>>,
    #[serde(default, rename = "ephemeralContainerStatuses")]
    ephemeral_container_statuses: Option<Vec<RawContainerStatus>>,
    #[serde(default, rename = "readyReplicas")]
    ready_replicas: Option<u64>,
    #[serde(default, rename = "updatedReplicas")]
    updated_replicas: Option<u64>,
    #[serde(default, rename = "desiredNumberScheduled")]
    desired_number_scheduled: Option<u64>,
    #[serde(default, rename = "numberReady")]
    number_ready: Option<u64>,
    #[serde(default)]
    succeeded: Option<u64>,
    #[serde(default)]
    failed: Option<u64>,
    #[serde(default)]
    capacity: Option<BTreeMap<String, String>>,
    #[serde(default)]
    allocatable: Option<BTreeMap<String, String>>,
    #[serde(default)]
    addresses: Option<Vec<RawNodeAddress>>,
    #[serde(default)]
    conditions: Option<Vec<RawCondition>>,
    #[serde(default, rename = "nodeInfo")]
    node_info: Option<RawNodeInfo>,
    #[serde(default, rename = "currentReplicas")]
    current_replicas: Option<u64>,
    #[serde(default, rename = "desiredReplicas")]
    desired_replicas: Option<u64>,
    #[serde(default, rename = "currentHealthy")]
    current_healthy: Option<u64>,
    #[serde(default, rename = "desiredHealthy")]
    desired_healthy: Option<u64>,
    #[serde(default, rename = "disruptionsAllowed")]
    disruptions_allowed: Option<u64>,
}

#[derive(Deserialize)]
struct RawContainerStatus {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    ready: Option<bool>,
    #[serde(default, rename = "restartCount")]
    restart_count: Option<u64>,
    #[serde(default, rename = "containerID")]
    container_id: Option<String>,
    #[serde(default)]
    state: Option<RawContainerState>,
}

#[derive(Deserialize)]
struct RawContainerState {
    #[serde(default)]
    running: Option<RawRunningState>,
    #[serde(default)]
    waiting: Option<RawWaitingState>,
    #[serde(default)]
    terminated: Option<RawTerminatedState>,
}

#[derive(Deserialize)]
struct RawRunningState {
    #[serde(default, rename = "startedAt")]
    started_at: Option<String>,
}

#[derive(Deserialize)]
struct RawWaitingState {
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize)]
struct RawTerminatedState {
    #[serde(default)]
    reason: Option<String>,
    #[serde(default, rename = "exitCode")]
    exit_code: Option<i64>,
    #[serde(default, rename = "startedAt")]
    started_at: Option<String>,
    #[serde(default, rename = "finishedAt")]
    finished_at: Option<String>,
    #[serde(default, rename = "containerID")]
    container_id: Option<String>,
}

#[derive(Deserialize)]
struct RawCondition {
    #[serde(default, rename = "type")]
    cond_type: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

#[derive(Deserialize)]
struct RawNodeAddress {
    #[serde(default, rename = "type")]
    addr_type: Option<String>,
    #[serde(default)]
    address: Option<String>,
}

#[derive(Deserialize)]
struct RawNodeInfo {
    #[serde(default, rename = "kubeletVersion")]
    kubelet_version: Option<String>,
    #[serde(default, rename = "kernelVersion")]
    kernel_version: Option<String>,
}

/// Parses a `kubectl get ... -o json` List or single resource into `ClusterResource` items
/// using typed deserialization so large annotations, `managedFields`, and CRD schemas are skipped without allocation.
pub fn parse_resource_list_json(json_str: &str) -> Vec<ClusterResource> {
    parse_paged_resource_list_json_with_rv(json_str, None)
        .map(|(items, _, _, _)| items)
        .unwrap_or_default()
}

/// Parses a Kubernetes `List` or single resource JSON into `(items, continue_token, remaining_item_count)`.
pub fn parse_paged_resource_list_json(
    json_str: &str,
    fallback_kind: Option<&str>,
) -> Option<(Vec<ClusterResource>, String, Option<u64>)> {
    parse_paged_resource_list_json_with_rv(json_str, fallback_kind)
        .map(|(items, cont, rem, _)| (items, cont, rem))
}

/// Parses a Kubernetes `List` or single resource JSON into `(items, continue_token, remaining_item_count, resource_version)`.
pub fn parse_paged_resource_list_json_with_rv(
    json_str: &str,
    fallback_kind: Option<&str>,
) -> Option<(Vec<ClusterResource>, String, Option<u64>, String)> {
    let parsed = serde_json::from_str::<RawResourceList>(json_str).ok()?;
    let list_meta = parsed.metadata.unwrap_or_default();
    let cont = list_meta.continue_token.unwrap_or_default();
    let remaining = list_meta.remaining_item_count;
    let mut rv = list_meta.resource_version.unwrap_or_default();
    let inferred_from_list = parsed
        .kind
        .as_deref()
        .and_then(|k| k.strip_suffix("List"))
        .filter(|k| !k.is_empty());
    let force_cr_kind = fallback_kind.filter(|k| k.starts_with("CR:"));

    let mut out = Vec::new();
    if let Some(items) = parsed.items {
        for mut item in items {
            if let Some(cr_k) = force_cr_kind {
                item.kind = Some(cr_k.to_owned());
            } else if item.kind.as_deref().is_none_or(str::is_empty) {
                item.kind = fallback_kind.or(inferred_from_list).map(str::to_owned);
            }
            if let Some(res) = parse_raw_resource_item(item) {
                out.push(res);
            }
        }
    } else if parsed.kind.is_some()
        && let Some(mut single) = parsed.single
    {
        if rv.is_empty()
            && let Some(ref m) = single.metadata
            && let Some(ref srv) = m.resource_version
        {
            rv = srv.clone();
        }
        if let Some(cr_k) = force_cr_kind {
            single.kind = Some(cr_k.to_owned());
        } else if single.kind.is_none() {
            single.kind = parsed.kind.or_else(|| fallback_kind.map(str::to_owned));
        }
        if let Some(res) = parse_raw_resource_item(single) {
            out.push(res);
        }
    }
    Some((out, cont, remaining, rv))
}

fn parse_raw_resource_item(item: RawResourceItem) -> Option<ClusterResource> {
    let kind = item.kind?;
    let meta = item.metadata?;
    let name = meta.name?;
    let namespace = meta.namespace.unwrap_or_default();
    let age = meta.creation_timestamp.unwrap_or_default();
    let spec = item.spec.unwrap_or(RawSpec {
        node_name: None,
        replicas: None,
        completions: None,
        schedule: None,
        suspend: None,
        service_type: None,
        cluster_ip: None,
        ingress_class_name: None,
        rules: None,
        storage_class_name: None,
        resources: None,
        group: None,
        scope: None,
        names: None,
        containers: None,
        init_containers: None,
        ephemeral_containers: None,
        template: None,
        ports: None,
        min_replicas: None,
        max_replicas: None,
        scale_target_ref: None,
        min_available: None,
        max_unavailable: None,
        holder_identity: None,
        controller: None,
        capacity: None,
        pv_reclaim_policy: None,
        unschedulable: None,
        taints: None,
    });
    let st = item.status.unwrap_or(RawStatus {
        phase: None,
        pod_ip: None,
        host_ip: None,
        qos_class: None,
        container_statuses: None,
        init_container_statuses: None,
        ephemeral_container_statuses: None,
        ready_replicas: None,
        updated_replicas: None,
        desired_number_scheduled: None,
        number_ready: None,
        succeeded: None,
        failed: None,
        capacity: None,
        allocatable: None,
        addresses: None,
        conditions: None,
        node_info: None,
        current_replicas: None,
        desired_replicas: None,
        current_healthy: None,
        desired_healthy: None,
        disruptions_allowed: None,
    });

    let (mut controlled_by_kind, mut controlled_by_name) = meta
        .owner_references
        .and_then(|mut v| v.pop())
        .map(|o| (o.kind.unwrap_or_default(), o.name.unwrap_or_default()))
        .unwrap_or_default();
    let raw_labels_map = meta.labels.unwrap_or_default();
    let labels: Vec<String> = raw_labels_map
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();

    let mut containers = Vec::new();
    let mut container_states = Vec::new();
    let mut ports = Vec::new();
    let mut data_keys = Vec::new();
    let mut status = "Active".to_owned();
    let mut ready = String::new();
    let mut detail = String::new();
    let mut restarts = 0u64;
    let mut pod_ip = st.pod_ip.clone().unwrap_or_default();
    let mut host_ip = st.host_ip.clone().unwrap_or_default();
    let mut node_name = spec.node_name.clone().unwrap_or_default();
    let mut qos = st.qos_class.clone().unwrap_or_default();
    let mut cpu = String::new();
    let mut memory = String::new();

    let carr = spec.containers.as_ref().or_else(|| {
        spec.template
            .as_ref()
            .and_then(|t| t.spec.as_ref())
            .and_then(|s| s.containers.as_ref())
    });
    if let Some(carr) = carr {
        for c in carr {
            if let Some(ref cname) = c.name {
                containers.push(cname.clone());
            }
            if let Some(ref parr) = c.ports {
                for p in parr {
                    if let Some(cp) = p.container_port {
                        ports.push(cp.to_string());
                    }
                }
            }
        }
    }
    let mut sidecar_names = std::collections::BTreeSet::new();
    let init_arr = spec.init_containers.as_ref().or_else(|| {
        spec.template
            .as_ref()
            .and_then(|t| t.spec.as_ref())
            .and_then(|s| s.init_containers.as_ref())
    });
    if let Some(init_arr) = init_arr {
        for c in init_arr {
            if let Some(ref cname) = c.name {
                containers.push(cname.clone());
                if c.restart_policy.as_deref() == Some("Always") {
                    sidecar_names.insert(cname.clone());
                }
            }
        }
    }
    if let Some(ref ephem_arr) = spec.ephemeral_containers {
        for c in ephem_arr {
            if let Some(ref cname) = c.name
                && !containers.contains(cname)
            {
                containers.push(cname.clone());
            }
        }
    }

    match kind.as_str() {
        "Pod" => {
            status = st.phase.unwrap_or_else(|| "Unknown".into());
            if let Some(cs_arr) = st.container_statuses {
                let total = cs_arr.len();
                let ready_count = cs_arr.iter().filter(|cs| cs.ready.unwrap_or(false)).count();
                ready = format!("{ready_count}/{total}");
                restarts = cs_arr.iter().filter_map(|cs| cs.restart_count).sum();
                for cs in &cs_arr {
                    let cname = cs.name.clone().unwrap_or_default();
                    let cready = cs.ready.unwrap_or(false);
                    let mut cfinished = String::new();
                    let mut cexit = None;
                    let mut cid = cs.container_id.clone().unwrap_or_default();
                    let (cstate, creason, cstarted) = match cs.state.as_ref() {
                        Some(s) if s.running.is_some() => (
                            "running".to_owned(),
                            String::new(),
                            s.running
                                .as_ref()
                                .and_then(|r| r.started_at.clone())
                                .unwrap_or_default(),
                        ),
                        Some(s) if s.waiting.is_some() => (
                            "waiting".to_owned(),
                            s.waiting
                                .as_ref()
                                .and_then(|w| w.reason.clone())
                                .unwrap_or_default(),
                            String::new(),
                        ),
                        Some(s) if s.terminated.is_some() => {
                            let term = s.terminated.as_ref().unwrap();
                            cexit = term.exit_code;
                            cfinished = term.finished_at.clone().unwrap_or_default();
                            if let Some(ref tc) = term.container_id {
                                cid = tc.clone();
                            }
                            let r = term.reason.clone().unwrap_or_else(|| {
                                if term.exit_code == Some(0) {
                                    "Completed".into()
                                } else {
                                    "Error".into()
                                }
                            });
                            let started = term.started_at.clone().unwrap_or_default();
                            ("terminated".to_owned(), r, started)
                        }
                        _ => ("unknown".to_owned(), String::new(), String::new()),
                    };
                    if cstate == "waiting" && !creason.is_empty() {
                        status = creason.clone();
                    }
                    container_states.push(ContainerStateBadge {
                        name: cname,
                        state: cstate,
                        ready: cready,
                        reason: creason,
                        is_init: false,
                        is_ephemeral: false,
                        is_sidecar: false,
                        started_at: cstarted,
                        finished_at: cfinished,
                        exit_code: cexit,
                        container_id: cid,
                    });
                }
                if let Some(init_cs_arr) = st.init_container_statuses {
                    for ics in init_cs_arr {
                        let cname = ics.name.unwrap_or_default();
                        let cready = ics.ready.unwrap_or(false);
                        let is_sidecar = sidecar_names.contains(&cname);
                        let mut cfinished = String::new();
                        let mut cexit = None;
                        let mut cid = ics.container_id.clone().unwrap_or_default();
                        let (cstate, creason, cstarted) = match ics.state.as_ref() {
                            Some(s) if s.running.is_some() => (
                                "running".to_owned(),
                                String::new(),
                                s.running
                                    .as_ref()
                                    .and_then(|r| r.started_at.clone())
                                    .unwrap_or_default(),
                            ),
                            Some(s) if s.waiting.is_some() => (
                                "waiting".to_owned(),
                                s.waiting
                                    .as_ref()
                                    .and_then(|w| w.reason.clone())
                                    .unwrap_or_default(),
                                String::new(),
                            ),
                            Some(s) if s.terminated.is_some() => {
                                let term = s.terminated.as_ref().unwrap();
                                cexit = term.exit_code;
                                cfinished = term.finished_at.clone().unwrap_or_default();
                                if let Some(ref tc) = term.container_id {
                                    cid = tc.clone();
                                }
                                (
                                    "terminated".to_owned(),
                                    term.reason.clone().unwrap_or_else(|| "Completed".into()),
                                    term.started_at.clone().unwrap_or_default(),
                                )
                            }
                            _ => ("terminated".to_owned(), "Completed".into(), String::new()),
                        };
                        container_states.push(ContainerStateBadge {
                            name: cname,
                            state: cstate,
                            ready: cready,
                            reason: creason,
                            is_init: true,
                            is_ephemeral: false,
                            is_sidecar,
                            started_at: cstarted,
                            finished_at: cfinished,
                            exit_code: cexit,
                            container_id: cid,
                        });
                    }
                }
                if let Some(ephem_cs_arr) = st.ephemeral_container_statuses {
                    for ecs in ephem_cs_arr {
                        let cname = ecs.name.unwrap_or_default();
                        let mut cfinished = String::new();
                        let mut cexit = None;
                        let mut cid = ecs.container_id.clone().unwrap_or_default();
                        let (cstate, creason, cstarted) = match ecs.state.as_ref() {
                            Some(s) if s.running.is_some() => (
                                "running".to_owned(),
                                String::new(),
                                s.running
                                    .as_ref()
                                    .and_then(|r| r.started_at.clone())
                                    .unwrap_or_default(),
                            ),
                            Some(s) if s.waiting.is_some() => (
                                "waiting".to_owned(),
                                s.waiting
                                    .as_ref()
                                    .and_then(|w| w.reason.clone())
                                    .unwrap_or_default(),
                                String::new(),
                            ),
                            Some(s) if s.terminated.is_some() => {
                                let term = s.terminated.as_ref().unwrap();
                                cexit = term.exit_code;
                                cfinished = term.finished_at.clone().unwrap_or_default();
                                if let Some(ref tc) = term.container_id {
                                    cid = tc.clone();
                                }
                                (
                                    "terminated".to_owned(),
                                    term.reason.clone().unwrap_or_else(|| "Completed".into()),
                                    term.started_at.clone().unwrap_or_default(),
                                )
                            }
                            _ => ("terminated".to_owned(), "Completed".into(), String::new()),
                        };
                        container_states.push(ContainerStateBadge {
                            name: cname,
                            state: cstate.clone(),
                            ready: cstate == "running",
                            reason: creason,
                            is_init: false,
                            is_ephemeral: true,
                            is_sidecar: false,
                            started_at: cstarted,
                            finished_at: cfinished,
                            exit_code: cexit,
                            container_id: cid,
                        });
                    }
                }
                let node = spec.node_name.as_deref().unwrap_or("");
                let pip = st.pod_ip.as_deref().unwrap_or("");
                detail = match (node.is_empty(), pip.is_empty()) {
                    (false, false) => format!("restarts: {restarts} • {pip} on {node}"),
                    (false, true) => format!("restarts: {restarts} • {node}"),
                    _ => format!("restarts: {restarts}"),
                };
            } else {
                ready = format!("0/{}", containers.len());
            }
        }
        "Deployment" | "StatefulSet" | "ReplicaSet" => {
            let desired = spec.replicas.unwrap_or(1);
            let rdy = st.ready_replicas.unwrap_or(0);
            let updated = st.updated_replicas.unwrap_or(rdy);
            ready = format!("{rdy}/{desired}");
            status = if rdy == desired {
                "Ready".into()
            } else if rdy > 0 {
                "Progressing".into()
            } else {
                "Scaling".into()
            };
            detail = format!("up-to-date: {updated}");
        }
        "DaemonSet" => {
            let desired = st.desired_number_scheduled.unwrap_or(0);
            let rdy = st.number_ready.unwrap_or(0);
            ready = format!("{rdy}/{desired}");
            status = if rdy == desired {
                "Ready".into()
            } else {
                "Degraded".into()
            };
        }
        "Job" => {
            let completions = spec.completions.unwrap_or(1);
            let succeeded = st.succeeded.unwrap_or(0);
            let failed = st.failed.unwrap_or(0);
            ready = format!("{succeeded}/{completions}");
            status = if succeeded >= completions {
                "Complete".into()
            } else if failed > 0 {
                "Failed".into()
            } else {
                "Running".into()
            };
        }
        "CronJob" => {
            let schedule = spec.schedule.as_deref().unwrap_or("* * * * *");
            let suspended = spec.suspend.unwrap_or(false);
            status = if suspended {
                "Suspended".into()
            } else {
                "Scheduled".into()
            };
            ready = schedule.to_owned();
            detail = format!("schedule: {schedule}");
        }
        "Service" => {
            let svc_type = spec.service_type.unwrap_or_else(|| "ClusterIP".into());
            let cluster_ip = spec.cluster_ip.unwrap_or_else(|| "None".into());
            status = svc_type;
            pod_ip = cluster_ip.clone();
            if let Some(parr) = spec.ports {
                for p in parr {
                    if let Some(port) = p.port {
                        let proto = p.protocol.as_deref().unwrap_or("TCP");
                        ports.push(port.to_string());
                        if !ready.is_empty() {
                            ready.push_str(", ");
                        }
                        ready.push_str(&format!("{port}/{proto}"));
                    }
                }
            }
            detail = format!("ClusterIP: {cluster_ip}");
        }
        "Ingress" => {
            let class = spec.ingress_class_name.unwrap_or_else(|| "default".into());
            let hosts: Vec<String> = spec
                .rules
                .map(|rules| rules.into_iter().filter_map(|r| r.host).collect())
                .unwrap_or_default();
            status = format!("Class: {class}");
            detail = if hosts.is_empty() {
                "*".into()
            } else {
                hosts.join(", ")
            };
        }
        "ConfigMap" | "Secret" => {
            if let Some(obj) = item.data {
                data_keys.extend(obj.into_keys());
            }
            if let Some(obj) = item.string_data {
                for k in obj.into_keys() {
                    if !data_keys.contains(&k) {
                        data_keys.push(k);
                    }
                }
            }
            data_keys.sort();
            ready = format!("{} keys", data_keys.len());
            if kind == "Secret" {
                status = item.secret_type.unwrap_or_else(|| "Opaque".into());
            } else {
                status = "ConfigMap".into();
            }
            detail = data_keys
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
        }
        "PersistentVolumeClaim" => {
            status = st.phase.unwrap_or_else(|| "Pending".into());
            let cap = st
                .capacity
                .as_ref()
                .and_then(|m| m.get("storage"))
                .or_else(|| {
                    spec.resources
                        .as_ref()
                        .and_then(|r| r.requests.as_ref())
                        .and_then(|m| m.get("storage"))
                })
                .cloned()
                .unwrap_or_default();
            let sc = spec.storage_class_name.unwrap_or_default();
            node_name = sc.clone();
            ready = cap;
            detail = if sc.is_empty() {
                String::new()
            } else {
                format!("storageClass: {sc}")
            };
        }
        "StorageClass" => {
            ready = item.provisioner.unwrap_or_default();
            status = item.reclaim_policy.unwrap_or_else(|| "Delete".into());
            detail = item
                .volume_binding_mode
                .unwrap_or_else(|| "Immediate".into());
        }
        "PersistentVolume" => {
            status = st.phase.unwrap_or_else(|| "Available".into());
            let sc = spec.storage_class_name.unwrap_or_default();
            node_name = sc.clone();
            let cap = spec
                .capacity
                .as_ref()
                .and_then(|m| m.get("storage"))
                .cloned()
                .unwrap_or_default();
            ready = cap;
            let reclaim = spec.pv_reclaim_policy.unwrap_or_default();
            detail = match (sc.is_empty(), reclaim.is_empty()) {
                (false, false) => format!("storageClass: {sc} • {reclaim}"),
                (false, true) => format!("storageClass: {sc}"),
                (true, false) => reclaim,
                _ => String::new(),
            };
        }
        "Node" => {
            let unsched = spec.unschedulable.unwrap_or(false);
            qos = if unsched {
                "False".into()
            } else {
                "True".into()
            };
            let mut active_conds: Vec<String> = Vec::new();
            if let Some(arr) = st.conditions {
                for c in arr {
                    if c.status.as_deref() == Some("True")
                        && let Some(ct) = c.cond_type
                    {
                        active_conds.push(ct);
                    }
                }
            }
            if active_conds.is_empty() {
                active_conds.push("NotReady".into());
            }
            if unsched && !active_conds.iter().any(|c| c == "SchedulingDisabled") {
                active_conds.push("SchedulingDisabled".into());
            }
            status = active_conds.join(", ");
            if let Some(ni) = st.node_info {
                ready = ni.kubelet_version.unwrap_or_default();
                node_name = ni.kernel_version.unwrap_or_default();
            }
            if let Some(addrs) = st.addresses
                && let Some(ip) = addrs
                    .iter()
                    .find(|a| a.addr_type.as_deref() == Some("InternalIP"))
                    .and_then(|a| a.address.clone())
                    .or_else(|| addrs.first().and_then(|a| a.address.clone()))
            {
                pod_ip = ip.clone();
                detail = ip;
            }
            let mut roles: Vec<String> = Vec::new();
            for (k, v) in &raw_labels_map {
                if let Some(role) = k.strip_prefix("node-role.kubernetes.io/")
                    && !role.is_empty()
                {
                    roles.push(role.to_owned());
                } else if k == "kubernetes.io/role" && !v.is_empty() {
                    roles.push(v.clone());
                }
            }
            roles.sort();
            roles.dedup();
            host_ip = roles.join(", ");

            controlled_by_kind = raw_labels_map
                .get("node.kubernetes.io/instance-type")
                .or_else(|| raw_labels_map.get("beta.kubernetes.io/instance-type"))
                .cloned()
                .unwrap_or_default();
            controlled_by_name = raw_labels_map
                .get("kubernetes.azure.com/agentpool")
                .or_else(|| raw_labels_map.get("eks.amazonaws.com/nodegroup"))
                .or_else(|| raw_labels_map.get("cloud.google.com/gke-nodepool"))
                .or_else(|| raw_labels_map.get("karpenter.sh/nodepool"))
                .cloned()
                .unwrap_or_default();

            if let Some(taints) = spec.taints {
                containers = taints
                    .into_iter()
                    .filter_map(|t| {
                        let key = t.key?;
                        let effect = t.effect.unwrap_or_default();
                        match (t.value.filter(|v| !v.is_empty()), effect.is_empty()) {
                            (Some(val), false) => Some(format!("{key}={val}:{effect}")),
                            (Some(val), true) => Some(format!("{key}={val}")),
                            (None, false) => Some(format!("{key}:{effect}")),
                            (None, true) => Some(key),
                        }
                    })
                    .collect();
                restarts = containers.len() as u64;
            }
            let alloc_map = st.allocatable.as_ref().or(st.capacity.as_ref());
            if let Some(m) = alloc_map {
                let alloc_cpu = m.get("cpu").map(|v| parse_kube_cores(v)).unwrap_or(0.0);
                let alloc_mem = m.get("memory").map(|v| parse_kube_gi(v)).unwrap_or(0.0);
                if alloc_cpu > 0.0 {
                    cpu = format!("0.00/{alloc_cpu:.2}");
                }
                if alloc_mem > 0.0 {
                    memory = format!("0.0Gi/{alloc_mem:.1}Gi");
                }
            }
        }
        "Namespace" => {
            status = st.phase.unwrap_or_else(|| "Active".into());
        }
        "CustomResourceDefinition" => {
            status = spec.scope.unwrap_or_default();
            ready = spec.names.and_then(|n| n.kind).unwrap_or_default();
            detail = spec.group.unwrap_or_default();
        }
        "HorizontalPodAutoscaler" => {
            let min_p = spec.min_replicas.unwrap_or(1);
            let max_p = spec.max_replicas.unwrap_or(1);
            let cur_p = st.current_replicas.unwrap_or(0);
            let des_p = st.desired_replicas.unwrap_or(0);
            ready = format!("{min_p}/{max_p} (current: {cur_p})");
            status = format!("{cur_p}/{des_p}");
            if let Some(t) = spec.scale_target_ref {
                let tk = t.kind.unwrap_or_default();
                let tn = t.name.unwrap_or_default();
                if !tn.is_empty() {
                    detail = format!("{tk}/{tn}");
                }
            }
        }
        "PodDisruptionBudget" => {
            let min_av = spec
                .min_available
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| v.to_string())
                })
                .unwrap_or_else(|| "N/A".into());
            let max_un = spec
                .max_unavailable
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| v.to_string())
                })
                .unwrap_or_else(|| "N/A".into());
            let cur_h = st.current_healthy.unwrap_or(0);
            let des_h = st.desired_healthy.unwrap_or(0);
            let dis_a = st.disruptions_allowed.unwrap_or(0);
            ready = format!("min: {min_av}, maxUnavail: {max_un}");
            status = format!("{cur_h}/{des_h}");
            detail = format!("allowed disruptions: {dis_a}");
        }
        "PriorityClass" => {
            ready = item.value.unwrap_or(0).to_string();
            status = if item.global_default.unwrap_or(false) {
                "True".into()
            } else {
                "False".into()
            };
        }
        "RuntimeClass" => {
            ready = item.handler.unwrap_or_default();
        }
        "Lease" => {
            ready = spec.holder_identity.unwrap_or_default();
        }
        "IngressClass" => {
            ready = spec.controller.unwrap_or_default();
        }
        "EndpointSlice" => {
            ready = item.address_type.unwrap_or_else(|| "IPv4".into());
        }
        "MutatingWebhookConfiguration" | "ValidatingWebhookConfiguration" => {
            let cnt = item.webhooks.map(|w| w.len()).unwrap_or(0);
            ready = cnt.to_string();
        }
        "Role" | "ClusterRole" => {
            let cnt = item.rules.map(|r| r.len()).unwrap_or(0);
            ready = format!("{cnt} rules");
        }
        "RoleBinding" | "ClusterRoleBinding" => {
            if let Some(rr) = item.role_ref {
                ready = rr.name.unwrap_or_default();
            }
            if let Some(subs) = item.subjects {
                let names: Vec<String> = subs.into_iter().filter_map(|s| s.name).collect();
                detail = names.join(", ");
            }
        }
        "ServiceAccount" => {
            let cnt = item.secrets.map(|s| s.len()).unwrap_or(0);
            ready = format!("{cnt} secrets");
        }
        other if other.starts_with("CR:") => {
            let crd_name = other.strip_prefix("CR:").unwrap_or(other);
            status = st
                .phase
                .or_else(|| {
                    st.conditions.and_then(|arr| {
                        arr.into_iter()
                            .find(|c| c.status.as_deref() == Some("True"))
                            .and_then(|c| c.cond_type)
                    })
                })
                .unwrap_or_else(|| "Active".into());
            detail = crd_name.to_owned();
            if controlled_by_name.is_empty() {
                controlled_by_name = crd_name.to_owned();
            }
        }
        _ => {}
    }

    Some(ClusterResource {
        kind,
        name,
        namespace,
        status,
        ready,
        age,
        detail,
        containers,
        ports,
        data_keys,
        container_states,
        cpu,
        memory,
        restarts,
        controlled_by_kind,
        controlled_by_name,
        pod_ip,
        host_ip,
        node_name,
        qos,
        labels,
    })
}

#[derive(Deserialize)]
struct RawEventList {
    #[serde(default)]
    metadata: Option<RawListMetadata>,
    #[serde(default)]
    items: Vec<RawEventItem>,
}

#[derive(Deserialize)]
struct RawEventItem {
    #[serde(default, rename = "involvedObject")]
    involved_object: Option<RawInvolvedObject>,
    #[serde(default)]
    metadata: Option<RawMetadata>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default, rename = "type")]
    event_type: Option<String>,
    #[serde(default)]
    count: Option<u64>,
    #[serde(default, rename = "lastTimestamp")]
    last_timestamp: Option<String>,
    #[serde(default, rename = "eventTime")]
    event_time: Option<String>,
}

#[derive(Deserialize)]
struct RawInvolvedObject {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

/// Parses `kubectl get events -o json` output into `ClusterEvent` items.
pub fn parse_events_json(json_str: &str) -> Vec<ClusterEvent> {
    parse_paged_events_json(json_str, Some(150))
        .map(|(items, _, _)| items)
        .unwrap_or_default()
}

pub fn parse_paged_events_json(
    json_str: &str,
    cap: Option<usize>,
) -> Option<(Vec<ClusterEvent>, String, Option<u64>)> {
    parse_paged_events_json_with_rv(json_str, cap).map(|(items, cont, rem, _)| (items, cont, rem))
}

pub fn parse_paged_events_json_with_rv(
    json_str: &str,
    cap: Option<usize>,
) -> Option<(Vec<ClusterEvent>, String, Option<u64>, String)> {
    let parsed = serde_json::from_str::<RawEventList>(json_str).ok()?;
    let list_meta = parsed.metadata.unwrap_or_default();
    let cont = list_meta.continue_token.unwrap_or_default();
    let remaining = list_meta.remaining_item_count;
    let rv = list_meta.resource_version.unwrap_or_default();
    let mut out = Vec::new();
    let iter: Box<dyn Iterator<Item = RawEventItem>> = if let Some(max_n) = cap {
        Box::new(parsed.items.into_iter().rev().take(max_n))
    } else {
        Box::new(parsed.items.into_iter())
    };
    for item in iter {
        let (obj_kind, obj_name) = match item.involved_object {
            Some(inv) => (
                inv.kind.unwrap_or_else(|| "Resource".into()),
                inv.name.unwrap_or_default(),
            ),
            None => ("Resource".into(), String::new()),
        };
        let (namespace, created) = match item.metadata {
            Some(m) => (
                m.namespace.unwrap_or_else(|| "default".into()),
                m.creation_timestamp.unwrap_or_default(),
            ),
            None => ("default".into(), String::new()),
        };
        let last_seen = item.last_timestamp.or(item.event_time).unwrap_or(created);
        out.push(ClusterEvent {
            kind: obj_kind,
            object: obj_name,
            namespace,
            reason: item.reason.unwrap_or_default(),
            message: item.message.unwrap_or_default(),
            event_type: item.event_type.unwrap_or_else(|| "Normal".into()),
            count: item.count.unwrap_or(1),
            last_seen,
        });
    }
    Some((out, cont, remaining, rv))
}

/// Parses `helm list -o json` output into `HelmRelease` items.
pub fn parse_helm_releases_json(json_str: &str) -> Vec<HelmRelease> {
    let Ok(arr) = serde_json::from_str::<Vec<Value>>(json_str) else {
        return Vec::new();
    };
    arr.into_iter()
        .filter_map(|v| {
            Some(HelmRelease {
                name: v.get("name")?.as_str()?.to_owned(),
                namespace: v
                    .get("namespace")
                    .and_then(Value::as_str)
                    .unwrap_or("default")
                    .to_owned(),
                revision: v
                    .get("revision")
                    .map(|r| {
                        r.as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| r.to_string())
                    })
                    .unwrap_or_default(),
                status: v
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned(),
                chart: v
                    .get("chart")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                app_version: v
                    .get("app_version")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                updated: v
                    .get("updated")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            })
        })
        .collect()
}

const CUSTOM_COLUMNS_SPEC: &str = "custom-columns=KIND:.kind,NS:.metadata.namespace,NAME:.metadata.name,PHASE:.status.phase,READY_LIST:.status.containerStatuses[*].ready,RESTARTS:.status.containerStatuses[*].restartCount,NODE:.spec.nodeName,POD_IP:.status.podIP,READY_REP:.status.readyReplicas,SPEC_REP:.spec.replicas,DS_READY:.status.numberReady,DS_DESIRED:.status.desiredNumberScheduled,JOB_SUCC:.status.succeeded,JOB_COMP:.spec.completions,SCHEDULE:.spec.schedule,SUSPEND:.spec.suspend,SVC_TYPE:.spec.type,CLUSTER_IP:.spec.clusterIP,ING_CLASS:.spec.ingressClassName,ING_HOSTS:.spec.rules[*].host,PVC_CAP:.status.capacity.storage,PVC_SC:.spec.storageClassName,CONTAINERS:.spec.containers[*].name,PORTS:.spec.containers[*].ports[*].containerPort,SVC_PORTS:.spec.ports[*].port,OWNER_KIND:.metadata.ownerReferences[0].kind,OWNER_NAME:.metadata.ownerReferences[0].name,QOS:.status.qosClass,AGE:.metadata.creationTimestamp";

fn clean_col(val: &str) -> &str {
    if val == "<none>" || val == "<unset>" || val == "<nil>" {
        ""
    } else {
        val
    }
}

/// Parses compact `custom-columns` lines into `ClusterResource` items.
/// This keeps cluster-wide `--all-namespaces` responses well under the 4 MiB process output cap even on large production clusters.
pub fn parse_custom_columns_resources(text: &str) -> Vec<ClusterResource> {
    let mut out = Vec::new();
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 26 {
            continue;
        }
        let n = cols.len();
        let has_owner_cols = n >= 29;
        let tail_cnt = if has_owner_cols { 14 } else { 11 };
        let kind = cols[0].to_owned();
        let namespace = clean_col(cols[1]).to_owned();
        let name = cols[2].to_owned();
        let phase = clean_col(cols[3]);
        let ready_list = clean_col(cols[4]);
        let restarts_list = clean_col(cols[5]);
        let node = clean_col(cols[6]);
        let pod_ip = clean_col(cols[7]);
        let ready_rep = clean_col(cols[8]);
        let spec_rep = clean_col(cols[9]);
        let ds_ready = clean_col(cols[10]);
        let ds_desired = clean_col(cols[11]);
        let job_succ = clean_col(cols[12]);
        let job_comp = clean_col(cols[13]);
        // `SCHEDULE` (index 14) may contain spaces (e.g. `*/5 * * * *`), while the last `tail_cnt` columns never contain spaces.
        let schedule_raw = cols[14..n - tail_cnt].join(" ");
        let schedule = clean_col(&schedule_raw);
        let suspend = clean_col(cols[n - tail_cnt]);
        let svc_type = clean_col(cols[n - tail_cnt + 1]);
        let cluster_ip = clean_col(cols[n - tail_cnt + 2]);
        let ing_class = clean_col(cols[n - tail_cnt + 3]);
        let ing_hosts = clean_col(cols[n - tail_cnt + 4]);
        let pvc_cap = clean_col(cols[n - tail_cnt + 5]);
        let pvc_sc = clean_col(cols[n - tail_cnt + 6]);
        let containers_raw = clean_col(cols[n - tail_cnt + 7]);
        let ports_raw = clean_col(cols[n - tail_cnt + 8]);
        let svc_ports_raw = clean_col(cols[n - tail_cnt + 9]);
        let (owner_kind, owner_name, qos) = if has_owner_cols {
            (
                clean_col(cols[n - 4]).to_owned(),
                clean_col(cols[n - 3]).to_owned(),
                clean_col(cols[n - 2]).to_owned(),
            )
        } else {
            (String::new(), String::new(), String::new())
        };
        let age = clean_col(cols[n - 1]).to_owned();

        let containers: Vec<String> = if containers_raw.is_empty() {
            Vec::new()
        } else {
            containers_raw
                .split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        };
        let mut ports: Vec<String> = if !ports_raw.is_empty() {
            ports_raw
                .split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        } else if !svc_ports_raw.is_empty() {
            svc_ports_raw
                .split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        } else {
            Vec::new()
        };
        ports.dedup();

        let mut status = "Active".to_owned();
        let mut ready = String::new();
        let mut detail = String::new();
        let mut restarts = 0u64;

        match kind.as_str() {
            "Pod" => {
                status = if phase.is_empty() {
                    "Unknown".into()
                } else {
                    phase.to_owned()
                };
                if !ready_list.is_empty() {
                    let parts: Vec<&str> = ready_list.split(',').collect();
                    let rdy_cnt = parts.iter().filter(|&&b| b == "true").count();
                    ready = format!("{rdy_cnt}/{}", parts.len());
                } else {
                    ready = format!("0/{}", containers.len());
                }
                restarts = restarts_list
                    .split(',')
                    .filter_map(|s| s.parse::<u64>().ok())
                    .sum();
                detail = match (node.is_empty(), pod_ip.is_empty()) {
                    (false, false) => format!("restarts: {restarts} • {pod_ip} on {node}"),
                    (false, true) => format!("restarts: {restarts} • {node}"),
                    _ => format!("restarts: {restarts}"),
                };
            }
            "Deployment" | "StatefulSet" | "ReplicaSet" => {
                let desired = spec_rep.parse::<u64>().unwrap_or(1);
                let rdy = ready_rep.parse::<u64>().unwrap_or(0);
                ready = format!("{rdy}/{desired}");
                status = if rdy == desired {
                    "Ready".into()
                } else if rdy > 0 {
                    "Progressing".into()
                } else {
                    "Scaling".into()
                };
                detail = format!("replicas: {rdy}/{desired}");
            }
            "DaemonSet" => {
                let desired = ds_desired.parse::<u64>().unwrap_or(0);
                let rdy = ds_ready.parse::<u64>().unwrap_or(0);
                ready = format!("{rdy}/{desired}");
                status = if rdy == desired {
                    "Ready".into()
                } else {
                    "Degraded".into()
                };
                detail = format!("pods: {rdy}/{desired}");
            }
            "Job" => {
                let comp = job_comp.parse::<u64>().unwrap_or(1);
                let succ = job_succ.parse::<u64>().unwrap_or(0);
                ready = format!("{succ}/{comp}");
                status = if succ >= comp {
                    "Complete".into()
                } else {
                    "Running".into()
                };
            }
            "CronJob" => {
                status = if suspend == "true" {
                    "Suspended".into()
                } else {
                    "Scheduled".into()
                };
                ready = schedule.to_owned();
                detail = if schedule.is_empty() {
                    String::new()
                } else {
                    format!("schedule: {schedule}")
                };
            }
            "Service" => {
                status = if svc_type.is_empty() {
                    "ClusterIP".into()
                } else {
                    svc_type.to_owned()
                };
                ready = ports.join(", ");
                detail = if cluster_ip.is_empty() {
                    String::new()
                } else {
                    format!("ClusterIP: {cluster_ip}")
                };
            }
            "Ingress" => {
                status = if ing_class.is_empty() {
                    "Ingress".into()
                } else {
                    format!("Class: {ing_class}")
                };
                detail = if ing_hosts.is_empty() {
                    "*".into()
                } else {
                    ing_hosts.to_owned()
                };
            }
            "ConfigMap" => {
                status = "ConfigMap".into();
            }
            "PersistentVolumeClaim" => {
                status = if phase.is_empty() {
                    "Bound".into()
                } else {
                    phase.to_owned()
                };
                ready = pvc_cap.to_owned();
                detail = if pvc_sc.is_empty() {
                    String::new()
                } else {
                    format!("storageClass: {pvc_sc}")
                };
            }
            _ => {}
        }

        out.push(ClusterResource {
            kind,
            name,
            namespace,
            status,
            ready,
            age,
            detail,
            containers,
            ports,
            restarts,
            controlled_by_kind: owner_kind,
            controlled_by_name: owner_name,
            pod_ip: if !pod_ip.is_empty() {
                pod_ip.to_owned()
            } else {
                cluster_ip.to_owned()
            },
            node_name: if !node.is_empty() {
                node.to_owned()
            } else {
                pvc_sc.to_owned()
            },
            qos,
            ..Default::default()
        });
        if out.len() >= 25_000 {
            break;
        }
    }
    out
}

/// Parses `kubectl get pods [-A] -o wide --no-headers` Server-Side Table output.
/// Unlike `-o json` or `-o custom-columns`, `-o wide` uses Kubernetes Server-Side `Table`
/// (`application/json;as=Table`), reducing payload size by ~150x on large clusters.
pub fn parse_pods_wide_table(
    text: &str,
    all_namespaces: bool,
    default_ns: &str,
) -> Vec<ClusterResource> {
    let mut out = Vec::new();
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        let min_cols = if all_namespaces { 6 } else { 5 };
        if cols.len() < min_cols {
            continue;
        }
        let mut idx = 0usize;
        let namespace = if all_namespaces {
            let ns = clean_col(cols[idx]).to_owned();
            idx += 1;
            ns
        } else {
            default_ns.to_owned()
        };
        let name = cols[idx].to_owned();
        idx += 1;
        let ready = cols[idx].to_owned();
        idx += 1;
        let status_raw = cols[idx];
        idx += 1;
        let restarts_tok = cols.get(idx).copied().unwrap_or("0");
        let restarts = restarts_tok.parse::<u64>().unwrap_or(0);
        idx += 1;
        // K8s 1.22+ formats recent restarts as `3 (5m12s ago)` across whitespace tokens.
        if cols.get(idx).is_some_and(|t| t.starts_with('(')) {
            while let Some(&tok) = cols.get(idx) {
                idx += 1;
                if tok.ends_with(')') {
                    break;
                }
            }
        }
        let age = cols
            .get(idx)
            .copied()
            .map(clean_col)
            .unwrap_or("")
            .to_owned();
        let pod_ip = cols
            .get(idx + 1)
            .copied()
            .map(clean_col)
            .unwrap_or("")
            .to_owned();
        let node_name = cols
            .get(idx + 2)
            .copied()
            .map(clean_col)
            .unwrap_or("")
            .to_owned();

        let (rdy_cnt, total_cnt) = ready
            .split_once('/')
            .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)))
            .unwrap_or((0, 0));
        let mut container_states = Vec::with_capacity(total_cnt.min(16));
        let mut containers = Vec::with_capacity(total_cnt.min(16));
        let is_done = matches!(status_raw, "Completed" | "Succeeded");
        for i in 0..total_cnt.min(16) {
            let cname = format!("c{}", i + 1);
            containers.push(cname.clone());
            let (cstate, cready, creason) = if is_done {
                ("terminated".to_owned(), false, "Completed".to_owned())
            } else if i < rdy_cnt {
                ("running".to_owned(), true, String::new())
            } else {
                ("waiting".to_owned(), false, status_raw.to_owned())
            };
            container_states.push(ContainerStateBadge {
                name: cname,
                state: cstate,
                ready: cready,
                reason: creason,
                ..Default::default()
            });
        }
        let detail = match (node_name.is_empty(), pod_ip.is_empty()) {
            (false, false) => format!("restarts: {restarts} • {pod_ip} on {node_name}"),
            (false, true) => format!("restarts: {restarts} • {node_name}"),
            _ => format!("restarts: {restarts}"),
        };
        out.push(ClusterResource {
            kind: "Pod".into(),
            name,
            namespace,
            status: status_raw.to_owned(),
            ready,
            age,
            detail,
            containers,
            container_states,
            restarts,
            pod_ip,
            node_name,
            ..Default::default()
        });
        if out.len() >= 25_000 {
            break;
        }
    }
    out
}

/// Parses Server-Side Table (`--no-headers`) output for namespaced resources when `--all-namespaces` is active.
pub fn parse_server_table_resources(
    kind: &str,
    text: &str,
    all_namespaces: bool,
    default_ns: &str,
) -> Vec<ClusterResource> {
    if kind == "Pod" {
        return parse_pods_wide_table(text, all_namespaces, default_ns);
    }
    let mut out = Vec::new();
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        let min_cols = if all_namespaces { 3 } else { 2 };
        if cols.len() < min_cols {
            continue;
        }
        let base = if all_namespaces { 1 } else { 0 };
        let namespace = if all_namespaces {
            clean_col(cols[0]).to_owned()
        } else {
            default_ns.to_owned()
        };
        let name = cols[base].to_owned();
        let age = cols.last().copied().map(clean_col).unwrap_or("").to_owned();
        let mut status = "Active".to_owned();
        let mut ready = String::new();
        let mut detail = String::new();
        let mut pod_ip = String::new();
        let mut node_name = String::new();
        let mut restarts = 0u64;

        match kind {
            "Deployment" | "StatefulSet" => {
                ready = cols.get(base + 1).copied().unwrap_or("0/0").to_owned();
                let (rdy, des) = ready
                    .split_once('/')
                    .and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?)))
                    .unwrap_or((0, 0));
                status = if rdy == des {
                    "Ready".into()
                } else if rdy > 0 {
                    "Progressing".into()
                } else {
                    "Scaling".into()
                };
                detail = format!("replicas: {ready}");
            }
            "DaemonSet" | "ReplicaSet" | "ReplicationController" => {
                let des = cols
                    .get(base + 1)
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                let rdy = cols
                    .get(base + 3)
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                ready = format!("{rdy}/{des}");
                status = if rdy == des {
                    "Ready".into()
                } else if kind == "DaemonSet" {
                    "Degraded".into()
                } else if rdy > 0 {
                    "Progressing".into()
                } else {
                    "Scaling".into()
                };
                detail = format!("ready: {ready}");
            }
            "Job" => {
                let c1 = cols.get(base + 1).copied().unwrap_or("");
                let c2 = cols.get(base + 2).copied().unwrap_or("");
                if c1.contains('/') {
                    ready = c1.to_owned();
                    let (succ, comp) = ready
                        .split_once('/')
                        .and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?)))
                        .unwrap_or((0, 1));
                    status = if succ >= comp {
                        "Complete".into()
                    } else {
                        "Running".into()
                    };
                } else {
                    status = c1.to_owned();
                    ready = c2.to_owned();
                }
            }
            "CronJob" => {
                // NAMESPACE NAME SCHEDULE [TIMEZONE] SUSPEND ACTIVE LAST_SCHEDULE AGE
                if cols.len() >= base + 6 {
                    let suspend_idx = cols.len().saturating_sub(4);
                    let active_idx = cols.len().saturating_sub(3);
                    let sched_end = if suspend_idx > base + 1
                        && matches!(cols[suspend_idx - 1], "True" | "False")
                    {
                        suspend_idx - 1
                    } else {
                        suspend_idx
                    };
                    let schedule = if sched_end > base + 1 {
                        cols[base + 1..sched_end].join(" ")
                    } else {
                        cols.get(base + 1).copied().unwrap_or("").to_owned()
                    };
                    let suspend = cols.get(suspend_idx).copied().unwrap_or("False");
                    restarts = cols
                        .get(active_idx)
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(0);
                    status = if suspend.eq_ignore_ascii_case("true") {
                        "Suspended".into()
                    } else {
                        "Scheduled".into()
                    };
                    ready = schedule.clone();
                    detail = format!("schedule: {schedule}");
                }
            }
            "Service" => {
                status = cols
                    .get(base + 1)
                    .copied()
                    .unwrap_or("ClusterIP")
                    .to_owned();
                pod_ip = cols
                    .get(base + 2)
                    .copied()
                    .map(clean_col)
                    .unwrap_or("")
                    .to_owned();
                ready = cols
                    .get(base + 4)
                    .copied()
                    .map(clean_col)
                    .unwrap_or("")
                    .to_owned();
                detail = if pod_ip.is_empty() {
                    String::new()
                } else {
                    format!("ClusterIP: {pod_ip}")
                };
            }
            "Ingress" => {
                let class = cols.get(base + 1).copied().map(clean_col).unwrap_or("");
                let hosts = cols.get(base + 2).copied().map(clean_col).unwrap_or("*");
                status = if class.is_empty() {
                    "Ingress".into()
                } else {
                    format!("Class: {class}")
                };
                detail = hosts.to_owned();
            }
            "ConfigMap" => {
                let data_cnt = cols.get(base + 1).copied().unwrap_or("0");
                status = "ConfigMap".into();
                ready = format!("{data_cnt} keys");
                detail = format!("{data_cnt} keys");
            }
            "PersistentVolumeClaim" => {
                status = cols.get(base + 1).copied().unwrap_or("Bound").to_owned();
                ready = cols
                    .get(base + 3)
                    .copied()
                    .map(clean_col)
                    .unwrap_or("")
                    .to_owned();
                node_name = cols
                    .get(base + 5)
                    .copied()
                    .map(clean_col)
                    .unwrap_or("")
                    .to_owned();
                detail = if node_name.is_empty() {
                    String::new()
                } else {
                    format!("storageClass: {node_name}")
                };
            }
            _ => {
                if cols.len() > base + 2 {
                    ready = clean_col(cols[base + 1]).to_owned();
                }
            }
        }

        out.push(ClusterResource {
            kind: kind.to_owned(),
            name,
            namespace,
            status,
            ready,
            age,
            detail,
            pod_ip,
            node_name,
            restarts,
            ..Default::default()
        });
        if out.len() >= 25_000 {
            break;
        }
    }
    out
}

/// Populates `snap.namespaces` (`Vec<String>` for the Namespace Picker) using fast Server-Side Table output
/// without bloating `snap.resources` with thousands of `ClusterResource` structs when viewing other tabs.
pub fn fetch_namespaces_into_snapshot(
    project: &Project,
    cfg: &KubeConfigState,
    snap: &mut ClusterSnapshot,
) {
    let mut ns_args = cfg.base_args(false);
    ns_args.extend(["get".into(), "namespaces".into(), "--no-headers".into()]);
    if let Ok(out) = run_kubectl_timed(project, &ns_args, None, Some(10))
        && out.status == Some(0)
    {
        let mut names = Vec::new();
        for line in out.stdout.lines() {
            if let Some(name) = line.split_whitespace().next()
                && !name.is_empty()
            {
                names.push(name.to_owned());
            }
        }
        if !names.is_empty() {
            names.sort();
            names.dedup();
            snap.overview_counts
                .insert("Namespace".into(), names.len() as u64);
            snap.namespaces = names;
        }
    }
}

fn fetch_namespaces_table_fallback(
    project: &Project,
    cfg: &KubeConfigState,
    snap: &mut ClusterSnapshot,
) {
    let mut ns_args = cfg.base_args(false);
    ns_args.extend(["get".into(), "namespaces".into(), "--no-headers".into()]);
    if let Ok(out) = run_kubectl_timed(project, &ns_args, None, Some(15))
        && out.status == Some(0)
    {
        snap.connected = true;
        snap.error = None;
        snap.resources.retain(|r| r.kind != "Namespace");
        let mut names = Vec::new();
        for line in out.stdout.lines() {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if let Some(&name) = cols.first() {
                let status = cols.get(1).copied().unwrap_or("Active");
                let age = cols.get(2).copied().unwrap_or("");
                names.push(name.to_owned());
                snap.resources.push(ClusterResource {
                    kind: "Namespace".into(),
                    name: name.to_owned(),
                    status: status.to_owned(),
                    age: age.to_owned(),
                    ..Default::default()
                });
            }
        }
        if !names.is_empty() {
            names.sort();
            names.dedup();
            snap.overview_counts
                .insert("Namespace".into(), names.len() as u64);
            snap.namespaces = names;
        }
        let loaded = snap
            .resources
            .iter()
            .filter(|r| r.kind == "Namespace")
            .count() as u64;
        snap.page_info.insert(
            "Namespace".into(),
            CategoryPageInfo {
                category: "Namespace".into(),
                limit: loaded as u32,
                request_token: String::new(),
                continue_token: String::new(),
                remaining_count: None,
                loaded_count: loaded,
                page_index: 1,
                mode: "replace".into(),
                resource_version: String::new(),
                namespace_versions: BTreeMap::new(),
                watch_supported: false,
            },
        );
    }
}

const NODE_COLUMNS_SPEC: &str = "custom-columns=NAME:.metadata.name,TAINTS:.spec.taints[*].key,UNSCHED:.spec.unschedulable,VERSION:.status.nodeInfo.kubeletVersion,KERNEL:.status.nodeInfo.kernelVersion,IP:.status.addresses[?(@.type==\"InternalIP\")].address,ALLOC_CPU:.status.allocatable.cpu,ALLOC_MEM:.status.allocatable.memory,COND_TYPES:.status.conditions[*].type,COND_STATUS:.status.conditions[*].status,INSTANCE:.metadata.labels.node\\.kubernetes\\.io/instance-type,POOL:.metadata.labels.kubernetes\\.azure\\.com/agentpool,AGE:.metadata.creationTimestamp";

fn parse_kube_cores(raw: &str) -> f64 {
    let s = clean_col(raw.trim());
    if s.is_empty() {
        return 0.0;
    }
    if let Some(m) = s.strip_suffix('m') {
        m.parse::<f64>().unwrap_or(0.0) / 1000.0
    } else if let Some(n) = s.strip_suffix('n') {
        n.parse::<f64>().unwrap_or(0.0) / 1_000_000_000.0
    } else {
        s.parse::<f64>().unwrap_or(0.0)
    }
}

fn parse_kube_gi(raw: &str) -> f64 {
    let s = clean_col(raw.trim());
    if s.is_empty() {
        return 0.0;
    }
    if let Some(ki) = s.strip_suffix("Ki") {
        ki.parse::<f64>().unwrap_or(0.0) / (1024.0 * 1024.0)
    } else if let Some(mi) = s.strip_suffix("Mi") {
        mi.parse::<f64>().unwrap_or(0.0) / 1024.0
    } else if let Some(gi) = s.strip_suffix("Gi") {
        gi.parse::<f64>().unwrap_or(0.0)
    } else if let Some(ti) = s.strip_suffix("Ti") {
        ti.parse::<f64>().unwrap_or(0.0) * 1024.0
    } else {
        s.parse::<f64>().unwrap_or(0.0) / (1024.0 * 1024.0 * 1024.0)
    }
}

/// Fetches `Node` rows on demand using Server-Side custom-columns output + `kubectl top nodes`,
/// avoiding downloading megabytes of full Node `.status.images` JSON over the network.
pub fn fetch_nodes_into_snapshot(
    project: &Project,
    cfg: &KubeConfigState,
    snap: &mut ClusterSnapshot,
) {
    fetch_nodes_with_options(project, cfg, snap, FetchOptions::default());
}

/// Parses `kubectl get nodes -o wide --no-headers` Server-Side Table lines into `ClusterResource` items.
pub fn parse_nodes_wide_table(
    text: &str,
    prev_metrics: &BTreeMap<String, (String, String)>,
) -> Vec<ClusterResource> {
    let mut out = Vec::new();
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        let Some(&name) = cols.first() else {
            continue;
        };
        let status_raw = cols.get(1).copied().unwrap_or("Ready");
        let status = status_raw.replace(',', ", ");
        let roles = cols.get(2).copied().map(clean_col).unwrap_or("");
        let age = cols.get(3).copied().map(clean_col).unwrap_or("");
        let version = cols.get(4).copied().map(clean_col).unwrap_or("");
        let internal_ip = cols.get(5).copied().map(clean_col).unwrap_or("");
        let kernel = if cols.len() >= 9 {
            clean_col(cols[cols.len() - 2])
        } else {
            ""
        };
        let schedulable = if status_raw.contains("SchedulingDisabled") {
            "False"
        } else {
            "True"
        };
        let (cpu, memory) = prev_metrics.get(name).cloned().unwrap_or_default();
        out.push(ClusterResource {
            kind: "Node".into(),
            name: name.to_owned(),
            status,
            ready: version.to_owned(),
            age: age.to_owned(),
            detail: internal_ip.to_owned(),
            cpu,
            memory,
            pod_ip: internal_ip.to_owned(),
            host_ip: if roles == "agent" {
                String::new()
            } else {
                roles.to_owned()
            },
            node_name: kernel.to_owned(),
            qos: schedulable.to_owned(),
            ..Default::default()
        });
    }
    out
}

pub fn fetch_nodes_with_options(
    project: &Project,
    cfg: &KubeConfigState,
    snap: &mut ClusterSnapshot,
    opts: FetchOptions,
) {
    let prev_metrics: BTreeMap<String, (String, String)> = snap
        .resources
        .iter()
        .filter(|r| r.kind == "Node" && (!r.cpu.is_empty() || !r.memory.is_empty()))
        .map(|r| (r.name.clone(), (r.cpu.clone(), r.memory.clone())))
        .collect();

    let req_timeout = if opts.is_background_poll {
        Some(3)
    } else {
        Some(15)
    };

    // Always fetch Server-Side Table (`-o wide --no-headers`) first so large clusters (500+ nodes)
    // never download tens of megabytes of `.status.images` JSON from kube-apiserver.
    let mut wide_args = cfg.base_args(false);
    wide_args.extend([
        "get".into(),
        "nodes".into(),
        "-o".into(),
        "wide".into(),
        "--no-headers".into(),
    ]);
    let Ok(wide_out) = run_kubectl_timed(project, &wide_args, None, req_timeout) else {
        return;
    };
    if wide_out.status != Some(0) {
        return;
    }

    if wide_out.stdout.trim_start().starts_with('{') {
        snap.resources.retain(|r| r.kind != "Node");
        snap.resources
            .extend(parse_resource_list_json(&wide_out.stdout));
        return;
    }

    let wide_nodes = parse_nodes_wide_table(&wide_out.stdout, &prev_metrics);
    let node_cnt = wide_nodes.len();

    // Only enrich full per-node allocatable + `kubectl top nodes` on small-to-medium clusters (<= 80 nodes).
    if node_cnt == 0 || node_cnt > 80 || (opts.is_background_poll && !opts.include_metrics) {
        snap.resources.retain(|r| r.kind != "Node");
        snap.resources.extend(wide_nodes);
        return;
    }

    let mut top_map: BTreeMap<String, (f64, f64)> = BTreeMap::new();
    if opts.include_metrics {
        let mut top_args = cfg.base_args(false);
        top_args.extend(["top".into(), "nodes".into(), "--no-headers".into()]);
        if let Ok(top_out) = run_kubectl_timed(project, &top_args, None, Some(3))
            && top_out.status == Some(0)
        {
            for line in top_out.stdout.lines() {
                let cols: Vec<&str> = line.split_whitespace().collect();
                if cols.len() >= 4 {
                    let nname = cols[0].to_owned();
                    let u_cpu = parse_kube_cores(cols[1]);
                    let u_mem = parse_kube_gi(cols[3]);
                    top_map.insert(nname, (u_cpu, u_mem));
                }
            }
        }
    }

    let mut node_args = cfg.base_args(false);
    node_args.extend([
        "get".into(),
        "nodes".into(),
        "-o".into(),
        NODE_COLUMNS_SPEC.into(),
        "--no-headers".into(),
    ]);
    if let Ok(out) = run_kubectl_timed(project, &node_args, None, Some(4))
        && out.status == Some(0)
        && !out.stdout.trim_start().starts_with('{')
    {
        let roles_by_name: BTreeMap<String, String> = wide_nodes
            .iter()
            .map(|n| (n.name.clone(), n.host_ip.clone()))
            .collect();
        snap.resources.retain(|r| r.kind != "Node");
        for line in out.stdout.lines() {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() >= 13 {
                let name = cols[0];
                let taints_raw = clean_col(cols[1]);
                let unsched = clean_col(cols[2]);
                let version = clean_col(cols[3]);
                let kernel = clean_col(cols[4]);
                let internal_ip = clean_col(cols[5]);
                let alloc_cpu = parse_kube_cores(cols[6]);
                let alloc_mem = parse_kube_gi(cols[7]);
                let cond_types: Vec<&str> = clean_col(cols[8])
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .collect();
                let cond_stats: Vec<&str> = clean_col(cols[9])
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .collect();
                let instance_type = clean_col(cols[10]);
                let node_group = clean_col(cols[11]);
                let age = clean_col(cols[12]);

                let taints_list: Vec<String> = if taints_raw.is_empty() {
                    Vec::new()
                } else {
                    taints_raw
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect()
                };
                let taints_cnt = taints_list.len() as u64;
                let schedulable = if unsched == "true" { "False" } else { "True" };

                let mut active_conds: Vec<String> = Vec::new();
                for (ct, cs) in cond_types.iter().zip(cond_stats.iter()) {
                    if *cs == "True" {
                        active_conds.push((*ct).to_owned());
                    }
                }
                if active_conds.is_empty() {
                    active_conds.push("NotReady".into());
                }
                if unsched == "true" && !active_conds.iter().any(|c| c == "SchedulingDisabled") {
                    active_conds.push("SchedulingDisabled".into());
                }
                let status = active_conds.join(", ");

                let (cpu_str, mem_str) = if opts.include_metrics {
                    let (u_cpu, u_mem) = top_map.get(name).copied().unwrap_or((0.0, 0.0));
                    (
                        if alloc_cpu > 0.0 {
                            format!("{u_cpu:.2}/{alloc_cpu:.2}")
                        } else {
                            String::new()
                        },
                        if alloc_mem > 0.0 {
                            format!("{u_mem:.1}Gi/{alloc_mem:.1}Gi")
                        } else {
                            String::new()
                        },
                    )
                } else if let Some((p_cpu, p_mem)) = prev_metrics.get(name) {
                    (p_cpu.clone(), p_mem.clone())
                } else {
                    (String::new(), String::new())
                };

                snap.resources.push(ClusterResource {
                    kind: "Node".into(),
                    name: name.to_owned(),
                    status,
                    ready: version.to_owned(),
                    age: age.to_owned(),
                    detail: internal_ip.to_owned(),
                    containers: taints_list,
                    cpu: cpu_str,
                    memory: mem_str,
                    restarts: taints_cnt,
                    controlled_by_kind: instance_type.to_owned(),
                    controlled_by_name: node_group.to_owned(),
                    pod_ip: internal_ip.to_owned(),
                    host_ip: roles_by_name.get(name).cloned().unwrap_or_default(),
                    node_name: kernel.to_owned(),
                    qos: schedulable.to_owned(),
                    ..Default::default()
                });
            }
        }
    } else {
        snap.resources.retain(|r| r.kind != "Node");
        snap.resources.extend(wide_nodes);
    }
}

const CR_INSTANCE_COLUMNS_SPEC: &str = "custom-columns=KIND:.kind,NS:.metadata.namespace,NAME:.metadata.name,AGE:.metadata.creationTimestamp";

/// Parses `CR_INSTANCE_COLUMNS_SPEC` lines into `ClusterResource` items for a given CRD name.
pub fn parse_cr_instance_lines(text: &str, crd_name: &str) -> Vec<ClusterResource> {
    let mut out = Vec::new();
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() >= 4 {
            let cr_kind = clean_col(cols[0]);
            let ns = clean_col(cols[1]);
            let name = cols[2];
            let age = clean_col(cols[3]);
            out.push(ClusterResource {
                kind: format!("CR:{crd_name}"),
                name: name.to_owned(),
                namespace: ns.to_owned(),
                status: "Active".into(),
                ready: cr_kind.to_owned(),
                age: age.to_owned(),
                detail: crd_name.to_owned(),
                controlled_by_kind: cr_kind.to_owned(),
                controlled_by_name: crd_name.to_owned(),
                ..Default::default()
            });
        } else if cols.len() == 3 {
            let ns = clean_col(cols[0]);
            let name = cols[1];
            let age = clean_col(cols[2]);
            out.push(ClusterResource {
                kind: format!("CR:{crd_name}"),
                name: name.to_owned(),
                namespace: ns.to_owned(),
                status: "Active".into(),
                age: age.to_owned(),
                detail: crd_name.to_owned(),
                controlled_by_name: crd_name.to_owned(),
                ..Default::default()
            });
        } else if cols.len() == 2 {
            let name = cols[0];
            let age = clean_col(cols[1]);
            out.push(ClusterResource {
                kind: format!("CR:{crd_name}"),
                name: name.to_owned(),
                status: "Active".into(),
                age: age.to_owned(),
                detail: crd_name.to_owned(),
                controlled_by_name: crd_name.to_owned(),
                ..Default::default()
            });
        }
    }
    out
}

/// Fetches `CustomResourceDefinition` rows on demand using Server-Side Table output (`kubectl get crd --no-headers`),
/// avoiding downloading megabytes of OpenAPI v3 validation schemas.
pub fn fetch_crds_into_snapshot(
    project: &Project,
    cfg: &KubeConfigState,
    snap: &mut ClusterSnapshot,
) {
    let mut crd_args = cfg.base_args(false);
    crd_args.extend(["get".into(), "crd".into(), "--no-headers".into()]);
    if let Ok(out) = run_kubectl(project, &crd_args, None)
        && out.status == Some(0)
    {
        snap.resources
            .retain(|r| r.kind != "CustomResourceDefinition");
        for line in out.stdout.lines() {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() >= 6 {
                snap.resources.push(ClusterResource {
                    kind: "CustomResourceDefinition".into(),
                    name: cols[0].to_owned(),
                    detail: clean_col(cols[1]).to_owned(),
                    controlled_by_kind: clean_col(cols[2]).to_owned(),
                    status: clean_col(cols[3]).to_owned(),
                    ready: clean_col(cols[4]).to_owned(),
                    age: clean_col(cols[5]).to_owned(),
                    ..Default::default()
                });
            } else if cols.len() >= 4 {
                snap.resources.push(ClusterResource {
                    kind: "CustomResourceDefinition".into(),
                    name: cols[0].to_owned(),
                    status: cols[3].to_owned(),
                    ready: cols[2].to_owned(),
                    age: cols.get(4).copied().unwrap_or("").to_owned(),
                    detail: cols[1].to_owned(),
                    controlled_by_kind: cols[2].to_owned(),
                    ..Default::default()
                });
            } else if let Some(&name) = cols.first() {
                let (plural, group) = name.split_once('.').unwrap_or((name, ""));
                let age = cols.get(1).copied().unwrap_or("");
                snap.resources.push(ClusterResource {
                    kind: "CustomResourceDefinition".into(),
                    name: name.to_owned(),
                    status: "Namespaced".into(),
                    ready: "v1".into(),
                    age: age.to_owned(),
                    detail: group.to_owned(),
                    controlled_by_kind: plural.to_owned(),
                    ..Default::default()
                });
            }
        }
    }
}

/// Fetches `Secret` rows using Server-Side Table output (`kubectl get secrets --no-headers`),
/// avoiding downloading `.data` payloads over the network.
pub fn fetch_secrets_into_snapshot(
    project: &Project,
    cfg: &KubeConfigState,
    snap: &mut ClusterSnapshot,
) {
    if cfg.is_multi_namespace() {
        let active = cfg.active_namespaces();
        snap.resources.retain(|r| r.kind != "Secret");
        for ns in active.iter().take(12) {
            let sub_cfg = cfg.with_single_namespace(ns);
            let mut sub_snap = ClusterSnapshot::default();
            fetch_secrets_into_snapshot(project, &sub_cfg, &mut sub_snap);
            snap.resources.extend(sub_snap.resources);
        }
        return;
    }
    let mut sec_args = cfg.base_args(true);
    sec_args.extend(["get".into(), "secrets".into()]);
    cfg.push_scope_flags(&mut sec_args);
    sec_args.push("--no-headers".into());
    if let Ok(out) = run_kubectl(project, &sec_args, None)
        && out.status == Some(0)
    {
        snap.resources.retain(|r| r.kind != "Secret");
        for line in out.stdout.lines() {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() >= 5 {
                let ns = clean_col(cols[0]);
                let name = cols[1];
                let sec_type = cols[2];
                let data_cnt = cols[3];
                let age = cols[4];
                snap.resources.push(ClusterResource {
                    kind: "Secret".into(),
                    name: name.to_owned(),
                    namespace: ns.to_owned(),
                    status: sec_type.to_owned(),
                    ready: format!("{data_cnt} keys"),
                    age: age.to_owned(),
                    detail: format!("type: {sec_type}"),
                    ..Default::default()
                });
            } else if cols.len() == 4 {
                if cols[2].chars().all(|c| c.is_ascii_digit()) {
                    let name = cols[0];
                    let sec_type = cols[1];
                    let data_cnt = cols[2];
                    let age = cols[3];
                    snap.resources.push(ClusterResource {
                        kind: "Secret".into(),
                        name: name.to_owned(),
                        namespace: cfg.namespace.clone(),
                        status: sec_type.to_owned(),
                        ready: format!("{data_cnt} keys"),
                        age: age.to_owned(),
                        detail: format!("type: {sec_type}"),
                        ..Default::default()
                    });
                } else {
                    let ns = clean_col(cols[0]);
                    let name = cols[1];
                    let sec_type = cols[2];
                    let age = cols[3];
                    snap.resources.push(ClusterResource {
                        kind: "Secret".into(),
                        name: name.to_owned(),
                        namespace: ns.to_owned(),
                        status: sec_type.to_owned(),
                        ready: "Secret".into(),
                        age: age.to_owned(),
                        detail: format!("type: {sec_type}"),
                        ..Default::default()
                    });
                }
            }
        }
    }
}

/// Fetches cluster `Event` items on demand.
pub fn fetch_events_into_snapshot(
    project: &Project,
    cfg: &KubeConfigState,
    snap: &mut ClusterSnapshot,
) {
    if cfg.is_multi_namespace() {
        let mut combined = Vec::new();
        for ns in cfg.active_namespaces().iter().take(12) {
            let sub_cfg = cfg.with_single_namespace(ns);
            let mut ev_args = sub_cfg.base_args(true);
            ev_args.extend(["get".into(), "events".into(), "-o".into(), "json".into()]);
            if let Ok(out) = run_kubectl(project, &ev_args, None)
                && out.status == Some(0)
            {
                combined.extend(parse_events_json(&out.stdout));
            }
        }
        snap.events = combined;
        return;
    }
    let mut ev_args = cfg.base_args(true);
    ev_args.extend(["get".into(), "events".into()]);
    cfg.push_scope_flags(&mut ev_args);
    ev_args.extend(["-o".into(), "json".into()]);
    if let Ok(out) = run_kubectl(project, &ev_args, None)
        && out.status == Some(0)
    {
        snap.events = parse_events_json(&out.stdout);
    }
}

/// Fetches `HelmRelease` items on demand.
pub fn fetch_helm_into_snapshot(
    project: &Project,
    cfg: &KubeConfigState,
    snap: &mut ClusterSnapshot,
) {
    if cfg.is_multi_namespace() {
        let active = cfg.active_namespaces();
        if active.len() <= 8 {
            let mut combined = Vec::new();
            let mut any_ok = false;
            for ns in &active {
                let mut helm_args = Vec::new();
                if !cfg.kubeconfig.trim().is_empty() {
                    helm_args.push(format!("--kubeconfig={}", cfg.kubeconfig.trim()));
                }
                if !cfg.context.trim().is_empty() {
                    helm_args.push(format!("--kube-context={}", cfg.context.trim()));
                }
                helm_args.extend([
                    "list".into(),
                    "-n".into(),
                    ns.clone(),
                    "-o".into(),
                    "json".into(),
                ]);
                if let Ok(out) = project.run_process("helm", &helm_args, None)
                    && out.status == Some(0)
                {
                    any_ok = true;
                    combined.extend(parse_helm_releases_json(&out.stdout));
                }
            }
            if any_ok {
                snap.connected = true;
                snap.error = None;
                snap.helm_releases = combined;
            }
            return;
        }
        let mut helm_args = Vec::new();
        if !cfg.kubeconfig.trim().is_empty() {
            helm_args.push(format!("--kubeconfig={}", cfg.kubeconfig.trim()));
        }
        if !cfg.context.trim().is_empty() {
            helm_args.push(format!("--kube-context={}", cfg.context.trim()));
        }
        helm_args.extend(["list".into(), "-A".into(), "-o".into(), "json".into()]);
        if let Ok(out) = project.run_process("helm", &helm_args, None)
            && out.status == Some(0)
        {
            let allowed: std::collections::BTreeSet<String> = active.into_iter().collect();
            snap.connected = true;
            snap.error = None;
            snap.helm_releases = parse_helm_releases_json(&out.stdout)
                .into_iter()
                .filter(|r| allowed.contains(&r.namespace))
                .collect();
        }
        return;
    }
    let mut helm_args = Vec::new();
    if !cfg.kubeconfig.trim().is_empty() {
        helm_args.push(format!("--kubeconfig={}", cfg.kubeconfig.trim()));
    }
    if !cfg.context.trim().is_empty() {
        helm_args.push(format!("--kube-context={}", cfg.context.trim()));
    }
    helm_args.push("list".into());
    if cfg.all_namespaces {
        helm_args.push("-A".into());
    } else if !cfg.namespace.trim().is_empty() {
        helm_args.push("-n".into());
        helm_args.push(cfg.namespace.trim().to_owned());
    }
    helm_args.extend(["-o".into(), "json".into()]);
    if let Ok(out) = project.run_process("helm", &helm_args, None)
        && out.status == Some(0)
    {
        snap.connected = true;
        snap.error = None;
        snap.helm_releases = parse_helm_releases_json(&out.stdout);
    }
}

fn format_pod_memory_aligned(raw: &str) -> String {
    let s = raw.trim();
    if let Some(mi) = s.strip_suffix("Mi").and_then(|v| v.parse::<f64>().ok()) {
        if mi >= 1024.0 {
            format!("{:.1}Gi", mi / 1024.0)
        } else {
            format!("{mi:.1}Mi")
        }
    } else if let Some(ki) = s.strip_suffix("Ki").and_then(|v| v.parse::<f64>().ok()) {
        if ki >= 1024.0 * 1024.0 {
            format!("{:.1}Gi", ki / (1024.0 * 1024.0))
        } else if ki >= 1024.0 {
            format!("{:.1}Mi", ki / 1024.0)
        } else {
            format!("{ki:.1}Ki")
        }
    } else if let Some(gi) = s.strip_suffix("Gi").and_then(|v| v.parse::<f64>().ok()) {
        format!("{gi:.1}Gi")
    } else {
        s.to_owned()
    }
}

/// Enriches `Pod` entries in `snap.resources` with live CPU & Memory from `kubectl top pods --no-headers` when available.
pub fn enrich_pod_metrics(project: &Project, cfg: &KubeConfigState, snap: &mut ClusterSnapshot) {
    let pod_cnt = snap.resources.iter().filter(|r| r.kind == "Pod").count();
    if pod_cnt == 0 || pod_cnt >= 2_500 {
        return;
    }
    if cfg.is_multi_namespace() {
        let mut present_ns: Vec<String> = Vec::new();
        for r in snap.resources.iter().filter(|r| r.kind == "Pod") {
            if !r.namespace.is_empty() && !present_ns.contains(&r.namespace) {
                present_ns.push(r.namespace.clone());
            }
        }
        if present_ns.len() > MAX_MULTI_NAMESPACE_WATCHES {
            return;
        }
        let mut ns_metrics: BTreeMap<(String, String), (String, String)> = BTreeMap::new();
        for ns in &present_ns {
            let sub_cfg = cfg.with_single_namespace(ns);
            let mut args = sub_cfg.base_args(true);
            args.extend(["top".into(), "pods".into(), "--no-headers".into()]);
            let Ok(out) = run_kubectl_timed(project, &args, None, Some(3)) else {
                continue;
            };
            if out.status != Some(0) {
                continue;
            }
            for line in out.stdout.lines() {
                let cols: Vec<&str> = line.split_whitespace().collect();
                if cols.len() >= 3 {
                    let (name, raw_cpu, raw_mem) = if cols.len() >= 4 {
                        (cols[1], cols[2], cols[3])
                    } else {
                        (cols[0], cols[1], cols[2])
                    };
                    let cpu = if let Some(m) = raw_cpu
                        .strip_suffix('m')
                        .and_then(|v| v.parse::<f64>().ok())
                    {
                        format!("{:.3}", m / 1000.0)
                    } else {
                        raw_cpu.to_owned()
                    };
                    let mem = format_pod_memory_aligned(raw_mem);
                    ns_metrics.insert((ns.clone(), name.to_owned()), (cpu, mem));
                }
            }
        }
        for r in snap.resources.iter_mut().filter(|r| r.kind == "Pod") {
            if let Some((cpu, mem)) = ns_metrics.get(&(r.namespace.clone(), r.name.clone())) {
                r.cpu = cpu.clone();
                r.memory = mem.clone();
            }
        }
        return;
    }
    let mut args = cfg.base_args(true);
    args.extend(["top".into(), "pods".into()]);
    cfg.push_scope_flags(&mut args);
    args.push("--no-headers".into());
    let Ok(out) = run_kubectl_timed(project, &args, None, Some(3)) else {
        return;
    };
    if out.status != Some(0) {
        return;
    }
    let mut ns_metrics: BTreeMap<(String, String), (String, String)> = BTreeMap::new();
    let mut single_metrics: BTreeMap<String, (String, String)> = BTreeMap::new();
    for line in out.stdout.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() >= 4 {
            let ns = cols[0].to_owned();
            let name = cols[1].to_owned();
            let raw_cpu = cols[2];
            let cpu = if let Some(m) = raw_cpu
                .strip_suffix('m')
                .and_then(|v| v.parse::<f64>().ok())
            {
                format!("{:.3}", m / 1000.0)
            } else {
                raw_cpu.to_owned()
            };
            let mem = format_pod_memory_aligned(cols[3]);
            ns_metrics.insert((ns, name), (cpu, mem));
        } else if cols.len() == 3 {
            let name = cols[0].to_owned();
            let raw_cpu = cols[1];
            let cpu = if let Some(m) = raw_cpu
                .strip_suffix('m')
                .and_then(|v| v.parse::<f64>().ok())
            {
                format!("{:.3}", m / 1000.0)
            } else {
                raw_cpu.to_owned()
            };
            let mem = format_pod_memory_aligned(cols[2]);
            single_metrics.insert(name, (cpu, mem));
        }
    }
    for r in snap.resources.iter_mut().filter(|r| r.kind == "Pod") {
        if let Some((cpu, mem)) = ns_metrics
            .get(&(r.namespace.clone(), r.name.clone()))
            .or_else(|| single_metrics.get(&r.name))
        {
            r.cpu = cpu.clone();
            r.memory = mem.clone();
        }
    }
}

pub const LARGE_CLUSTER_NAMESPACE_THRESHOLD: usize = 150;

/// Scopes `cfg` and `snap` back to the active namespace when a cluster has more than
/// `LARGE_CLUSTER_NAMESPACE_THRESHOLD` namespaces during startup or multi-kind overview queries.
pub fn enforce_large_cluster_scope(cfg: &mut KubeConfigState, snap: &mut ClusterSnapshot) -> bool {
    if cfg.all_namespaces && snap.namespaces.len() > LARGE_CLUSTER_NAMESPACE_THRESHOLD {
        cfg.all_namespaces = false;
        snap.all_namespaces = false;
        let _ = storage::set("allNamespaces", "false");
        true
    } else {
        false
    }
}

/// Fetches the 10 namespaced workload/network/config resource kinds into `snap`.
pub fn fetch_workloads_into_snapshot(
    project: &Project,
    cfg: &KubeConfigState,
    snap: &mut ClusterSnapshot,
) {
    if cfg.is_multi_namespace() {
        let active = cfg.active_namespaces();
        snap.resources.retain(|r| {
            matches!(
                r.kind.as_str(),
                "Namespace" | "Node" | "CustomResourceDefinition" | "Secret"
            )
        });
        let mut any_ok = false;
        let mut last_err: Option<String> = None;
        for ns in active.iter().take(8) {
            let sub_cfg = cfg.with_single_namespace(ns);
            let mut res_args = sub_cfg.base_args(true);
            res_args.extend([
                "get".into(),
                "pods,deployments,statefulsets,daemonsets,replicasets,jobs,cronjobs,services,ingresses,configmaps,pvc".into(),
                "-o".into(),
                "json".into(),
                "--chunk-size=500".into(),
            ]);
            match run_kubectl(project, &res_args, None) {
                Ok(out) if out.status == Some(0) => {
                    any_ok = true;
                    if out.stdout.trim_start().starts_with('{') {
                        snap.resources.extend(parse_resource_list_json(&out.stdout));
                    } else {
                        snap.resources
                            .extend(parse_custom_columns_resources(&out.stdout));
                    }
                }
                Ok(out) => {
                    last_err = Some(first_line_or(&out.stderr, "kubectl get resources failed"));
                }
                Err(e) => {
                    last_err = Some(e.to_string());
                }
            }
        }
        if any_ok {
            snap.connected = true;
            snap.error = None;
            enrich_pod_metrics(project, cfg, snap);
        } else if let Some(err) = last_err {
            snap.error = Some(err);
        }
        return;
    }
    let mut eff_cfg = cfg.clone();
    if eff_cfg.all_namespaces && snap.namespaces.len() > LARGE_CLUSTER_NAMESPACE_THRESHOLD {
        eff_cfg.all_namespaces = false;
        snap.all_namespaces = false;
        let _ = storage::set("allNamespaces", "false");
    }
    let mut res_args = eff_cfg.base_args(true);
    res_args.extend([
        "get".into(),
        "pods,deployments,statefulsets,daemonsets,replicasets,jobs,cronjobs,services,ingresses,configmaps,pvc".into(),
    ]);
    eff_cfg.push_scope_flags(&mut res_args);
    if eff_cfg.all_namespaces {
        res_args.extend([
            "-o".into(),
            CUSTOM_COLUMNS_SPEC.into(),
            "--no-headers".into(),
        ]);
    } else {
        res_args.extend(["-o".into(), "json".into(), "--chunk-size=500".into()]);
    }
    match run_kubectl(project, &res_args, None) {
        Ok(out) if out.status == Some(0) => {
            snap.connected = true;
            snap.error = None;
            snap.resources.retain(|r| {
                matches!(
                    r.kind.as_str(),
                    "Namespace" | "Node" | "CustomResourceDefinition" | "Secret"
                )
            });
            if out.stdout.trim_start().starts_with('{') {
                snap.resources.extend(parse_resource_list_json(&out.stdout));
            } else {
                snap.resources
                    .extend(parse_custom_columns_resources(&out.stdout));
            }
            enrich_pod_metrics(project, &eff_cfg, snap);
        }
        Ok(out) => {
            snap.error = Some(first_line_or(&out.stderr, "kubectl get resources failed"));
        }
        Err(e) => {
            snap.error = Some(e.to_string());
        }
    }
}

/// Maps a Kubernetes resource `Kind` to `(kubectl_plural_name, is_cluster_scoped, prefer_json_in_all_ns)`.
pub fn kubectl_resource_info(kind: &str) -> (&'static str, bool, bool) {
    match kind {
        "Pod" => ("pods", false, true),
        "Deployment" => ("deployments", false, false),
        "StatefulSet" => ("statefulsets", false, false),
        "DaemonSet" => ("daemonsets", false, false),
        "ReplicaSet" => ("replicasets", false, false),
        "ReplicationController" => ("replicationcontrollers", false, false),
        "Job" => ("jobs", false, false),
        "CronJob" => ("cronjobs", false, false),
        "Service" => ("services", false, false),
        "Endpoints" => ("endpoints", false, false),
        "EndpointSlice" => ("endpointslices", false, false),
        "Ingress" => ("ingresses", false, true),
        "IngressClass" => ("ingressclasses", true, true),
        "NetworkPolicy" => ("networkpolicies", false, true),
        "ConfigMap" => ("configmaps", false, false),
        "Secret" => ("secrets", false, false),
        "ResourceQuota" => ("resourcequotas", false, true),
        "LimitRange" => ("limitranges", false, true),
        "HorizontalPodAutoscaler" => ("horizontalpodautoscalers", false, true),
        "PodDisruptionBudget" => ("poddisruptionbudgets", false, true),
        "PriorityClass" => ("priorityclasses", true, true),
        "RuntimeClass" => ("runtimeclasses", true, true),
        "Lease" => ("leases", false, false),
        "MutatingWebhookConfiguration" => ("mutatingwebhookconfigurations", true, true),
        "ValidatingWebhookConfiguration" => ("validatingwebhookconfigurations", true, true),
        "PersistentVolumeClaim" => ("pvc", false, false),
        "PersistentVolume" => ("pv", true, true),
        "StorageClass" => ("storageclasses", true, true),
        "Namespace" => ("namespaces", true, false),
        "Node" => ("nodes", true, false),
        "Event" => ("events", false, false),
        "ServiceAccount" => ("serviceaccounts", false, false),
        "ClusterRole" => ("clusterroles", true, true),
        "Role" => ("roles", false, false),
        "ClusterRoleBinding" => ("clusterrolebindings", true, true),
        "RoleBinding" => ("rolebindings", false, true),
        "CustomResourceDefinition" => ("crd", true, false),
        _ => ("pods", false, false),
    }
}

/// Percent-encodes a URL path segment or query parameter value per RFC 3986.
pub fn url_encode_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Maps a built-in Kubernetes resource `Kind` to `(api_prefix, plural, is_cluster_scoped)`.
pub fn k8s_api_endpoint_info(kind: &str) -> Option<(&'static str, &'static str, bool)> {
    match kind {
        "Pod" => Some(("/api/v1", "pods", false)),
        "ReplicationController" => Some(("/api/v1", "replicationcontrollers", false)),
        "Service" => Some(("/api/v1", "services", false)),
        "Endpoints" => Some(("/api/v1", "endpoints", false)),
        "ConfigMap" => Some(("/api/v1", "configmaps", false)),
        "Secret" => Some(("/api/v1", "secrets", false)),
        "ResourceQuota" => Some(("/api/v1", "resourcequotas", false)),
        "LimitRange" => Some(("/api/v1", "limitranges", false)),
        "PersistentVolumeClaim" => Some(("/api/v1", "persistentvolumeclaims", false)),
        "PersistentVolume" => Some(("/api/v1", "persistentvolumes", true)),
        "Namespace" => Some(("/api/v1", "namespaces", true)),
        "Node" => Some(("/api/v1", "nodes", true)),
        "Event" => Some(("/api/v1", "events", false)),
        "ServiceAccount" => Some(("/api/v1", "serviceaccounts", false)),
        "Deployment" => Some(("/apis/apps/v1", "deployments", false)),
        "StatefulSet" => Some(("/apis/apps/v1", "statefulsets", false)),
        "DaemonSet" => Some(("/apis/apps/v1", "daemonsets", false)),
        "ReplicaSet" => Some(("/apis/apps/v1", "replicasets", false)),
        "Job" => Some(("/apis/batch/v1", "jobs", false)),
        "CronJob" => Some(("/apis/batch/v1", "cronjobs", false)),
        "EndpointSlice" => Some(("/apis/discovery.k8s.io/v1", "endpointslices", false)),
        "Ingress" => Some(("/apis/networking.k8s.io/v1", "ingresses", false)),
        "IngressClass" => Some(("/apis/networking.k8s.io/v1", "ingressclasses", true)),
        "NetworkPolicy" => Some(("/apis/networking.k8s.io/v1", "networkpolicies", false)),
        "HorizontalPodAutoscaler" => {
            Some(("/apis/autoscaling/v2", "horizontalpodautoscalers", false))
        }
        "PodDisruptionBudget" => Some(("/apis/policy/v1", "poddisruptionbudgets", false)),
        "PriorityClass" => Some(("/apis/scheduling.k8s.io/v1", "priorityclasses", true)),
        "RuntimeClass" => Some(("/apis/node.k8s.io/v1", "runtimeclasses", true)),
        "Lease" => Some(("/apis/coordination.k8s.io/v1", "leases", false)),
        "MutatingWebhookConfiguration" => Some((
            "/apis/admissionregistration.k8s.io/v1",
            "mutatingwebhookconfigurations",
            true,
        )),
        "ValidatingWebhookConfiguration" => Some((
            "/apis/admissionregistration.k8s.io/v1",
            "validatingwebhookconfigurations",
            true,
        )),
        "StorageClass" => Some(("/apis/storage.k8s.io/v1", "storageclasses", true)),
        "ClusterRole" => Some(("/apis/rbac.authorization.k8s.io/v1", "clusterroles", true)),
        "Role" => Some(("/apis/rbac.authorization.k8s.io/v1", "roles", false)),
        "ClusterRoleBinding" => Some((
            "/apis/rbac.authorization.k8s.io/v1",
            "clusterrolebindings",
            true,
        )),
        "RoleBinding" => Some(("/apis/rbac.authorization.k8s.io/v1", "rolebindings", false)),
        _ => None,
    }
}

pub fn build_paged_raw_uri(
    api_prefix: &str,
    plural: &str,
    cluster_scoped: bool,
    cfg: &KubeConfigState,
    limit: u32,
    continue_token: &str,
) -> String {
    let base = if cluster_scoped || cfg.all_namespaces || cfg.namespace.trim().is_empty() {
        format!("{api_prefix}/{plural}")
    } else {
        let ns = url_encode_component(cfg.namespace.trim());
        format!("{api_prefix}/namespaces/{ns}/{plural}")
    };
    let clamped = limit.clamp(1, 500);
    if continue_token.trim().is_empty() {
        format!("{base}?limit={clamped}")
    } else {
        let enc_cont = url_encode_component(continue_token.trim());
        format!("{base}?limit={clamped}&continue={enc_cont}")
    }
}

#[derive(Deserialize)]
struct RawApiGroupDiscovery {
    #[serde(default, rename = "preferredVersion")]
    preferred_version: Option<RawApiGroupVersion>,
    #[serde(default)]
    versions: Vec<RawApiGroupVersion>,
}

#[derive(Deserialize)]
struct RawApiGroupVersion {
    #[serde(default)]
    version: String,
}

#[derive(Deserialize)]
struct RawApiResourceListDiscovery {
    #[serde(default)]
    resources: Vec<RawApiResourceEntry>,
}

#[derive(Deserialize)]
struct RawApiResourceEntry {
    #[serde(default)]
    name: String,
    #[serde(default)]
    namespaced: bool,
}

fn resolve_crd_api_endpoint(
    project: &Project,
    cfg: &KubeConfigState,
    snap: &mut ClusterSnapshot,
    crd_name: &str,
    req_timeout: Option<u32>,
) -> Option<(String, String, bool)> {
    let (plural, group) = crd_name.split_once('.')?;
    if plural.is_empty() || group.is_empty() {
        return None;
    }
    if let Some(existing) = snap
        .resources
        .iter()
        .find(|r| r.kind == "CustomResourceDefinition" && r.name == crd_name)
        && existing.labels.iter().any(|l| l == "discovered=true")
        && !existing.ready.is_empty()
    {
        let api_prefix = format!("/apis/{group}/{}", existing.ready);
        let is_cluster = existing.status.eq_ignore_ascii_case("Cluster");
        return Some((api_prefix, plural.to_owned(), is_cluster));
    }

    let mut grp_args = cfg.base_args(false);
    grp_args.extend(["get".into(), "--raw".into(), format!("/apis/{group}")]);
    let grp_out = run_kubectl_timed(project, &grp_args, None, req_timeout).ok()?;
    if grp_out.status != Some(0) || !grp_out.stdout.trim_start().starts_with('{') {
        return None;
    }
    let grp_disc = serde_json::from_str::<RawApiGroupDiscovery>(&grp_out.stdout).ok()?;
    let version = grp_disc
        .preferred_version
        .map(|pv| pv.version)
        .filter(|v| !v.is_empty())
        .or_else(|| {
            grp_disc
                .versions
                .into_iter()
                .map(|v| v.version)
                .find(|v| !v.is_empty())
        })?;

    let mut res_args = cfg.base_args(false);
    res_args.extend([
        "get".into(),
        "--raw".into(),
        format!("/apis/{group}/{version}"),
    ]);
    let mut is_cluster = false;
    if let Ok(res_out) = run_kubectl_timed(project, &res_args, None, req_timeout)
        && res_out.status == Some(0)
        && let Ok(list_disc) = serde_json::from_str::<RawApiResourceListDiscovery>(&res_out.stdout)
        && let Some(entry) = list_disc.resources.iter().find(|e| e.name == plural)
    {
        is_cluster = !entry.namespaced;
    }

    // Cache discovered version + scope across all CRDs in this API group so subsequent pages cost 0 extra calls.
    for r in snap
        .resources
        .iter_mut()
        .filter(|r| r.kind == "CustomResourceDefinition" && r.name == crd_name)
    {
        r.ready = version.clone();
        r.status = if is_cluster {
            "Cluster".into()
        } else {
            "Namespaced".into()
        };
        if !r.labels.iter().any(|l| l == "discovered=true") {
            r.labels.push("discovered=true".into());
        }
    }

    Some((
        format!("/apis/{group}/{version}"),
        plural.to_owned(),
        is_cluster,
    ))
}

struct FetchedPage<T> {
    items: Vec<T>,
    request_token: String,
    continue_token: String,
    remaining_count: Option<u64>,
    resource_version: String,
    namespace_versions: BTreeMap<String, String>,
    page_index: u32,
    append: bool,
    used_limit: u32,
}

enum RawPageOutcome<T> {
    Page(Box<FetchedPage<T>>),
    Unsupported,
    Failed(String),
}

fn parse_multi_ns_cursor(token: &str, ns_len: usize) -> Option<(usize, String)> {
    let rest = token.trim().strip_prefix("mns:")?;
    let (idx_str, inner) = rest.split_once(':')?;
    let idx = idx_str.parse::<usize>().ok()?;
    if idx < ns_len {
        Some((idx, inner.to_owned()))
    } else {
        None
    }
}

/// Executes a small batch (`<= 4`) of single-namespace `kubectl get --raw` calls concurrently via
/// `host::start` when `limit <= 100` (falling back seamlessly to `run_kubectl_timed` whenever `limit > 100`,
/// `host::start` is unavailable, or stdout approaches `Child`'s 1 MiB stream buffer).
fn run_raw_uri_batch(
    project: &Project,
    cfg: &KubeConfigState,
    uris: &[String],
    allow_parallel_children: bool,
    req_timeout: Option<u32>,
) -> Vec<Result<ProcessOutput, Error>> {
    let timeout_secs = req_timeout.unwrap_or(15);
    if !allow_parallel_children || uris.len() <= 1 {
        return uris
            .iter()
            .map(|uri| {
                let mut args = cfg.base_args(false);
                args.extend(["get".into(), "--raw".into(), uri.clone()]);
                run_kubectl_timed(project, &args, None, Some(timeout_secs))
            })
            .collect();
    }

    let mut spawned: Vec<Option<host::Child>> = Vec::with_capacity(uris.len());
    for uri in uris {
        let mut args = cfg.base_args(false);
        args.extend([
            "get".into(),
            "--raw".into(),
            uri.clone(),
            format!("--request-timeout={timeout_secs}s"),
        ]);
        let cmd = host::Command::new("kubectl").args(args);
        spawned.push(host::start(&cmd, Some(project)).ok());
    }

    let wait_ms = timeout_secs.saturating_mul(1_000).max(1_000);
    spawned
        .into_iter()
        .zip(uris.iter())
        .map(|(maybe_child, uri)| {
            if let Some(child) = maybe_child {
                let mut stdout = String::new();
                let mut stderr = String::new();
                let mut status: Option<i32> = None;
                let mut overflowed_ring = false;
                for _ in 0..8 {
                    match child.wait(wait_ms) {
                        Ok(chunk) => {
                            if chunk.stdout.len() >= 900 * 1024 {
                                overflowed_ring = true;
                                break;
                            }
                            stdout.push_str(&chunk.stdout);
                            stderr.push_str(&chunk.stderr);
                            if chunk.status.is_some() {
                                status = chunk.status;
                                break;
                            }
                        }
                        Err(_) => {
                            overflowed_ring = true;
                            break;
                        }
                    }
                }
                let _ = child.kill();
                if !overflowed_ring
                    && status.is_some()
                    && (status != Some(0) || stdout.trim_start().starts_with('{'))
                {
                    return Ok(ProcessOutput {
                        status,
                        stdout,
                        stderr,
                    });
                }
            }
            let mut args = cfg.base_args(false);
            args.extend(["get".into(), "--raw".into(), uri.clone()]);
            run_kubectl_timed(project, &args, None, Some(timeout_secs))
        })
        .collect()
}

fn fetch_raw_paged_multi_ns<T, F>(
    project: &Project,
    cfg: &KubeConfigState,
    endpoint: (&str, &str),
    opts: &FetchOptions,
    req_timeout: Option<u32>,
    mut parse_fn: F,
) -> RawPageOutcome<T>
where
    F: FnMut(&str) -> Option<(Vec<T>, String, Option<u64>, String)>,
{
    let (api_prefix, plural) = endpoint;
    let namespaces = cfg.active_namespaces();
    let ns_len = namespaces.len();
    if ns_len == 0 {
        return RawPageOutcome::Unsupported;
    }

    let limit = opts.limit.clamp(10, 500);
    let raw_req_tok = opts.continue_token.trim().to_owned();
    let (mut idx, mut inner_tok, req_tok, page_idx, append) = if raw_req_tok.is_empty() {
        (
            0usize,
            String::new(),
            String::new(),
            opts.page_index.max(1),
            opts.append,
        )
    } else if let Some((parsed_idx, parsed_inner)) = parse_multi_ns_cursor(&raw_req_tok, ns_len) {
        (
            parsed_idx,
            parsed_inner,
            raw_req_tok,
            opts.page_index.max(1),
            opts.append,
        )
    } else {
        (0usize, String::new(), String::new(), 1, false)
    };

    let mut collected: Vec<T> = Vec::new();
    let mut namespace_versions: BTreeMap<String, String> = BTreeMap::new();
    let mut next_continue = String::new();
    let mut rem_total: u64 = 0;
    let mut any_rem = false;
    let mut any_ok = false;
    let mut last_err: Option<String> = None;
    let eager_watch_drain = req_tok.is_empty() && !append && ns_len <= MAX_MULTI_NAMESPACE_WATCHES;
    let max_collect_cap = if eager_watch_drain {
        (WATCH_ELIGIBLE_MAX_ITEMS as usize).max(limit as usize)
    } else {
        limit as usize
    };

    // 1. If resuming mid-namespace via `mns:{idx}:{inner_tok}` with non-empty `inner_tok`, fetch that page first.
    if !inner_tok.is_empty() && idx < ns_len {
        let ns = &namespaces[idx];
        let sub_cfg = cfg.with_single_namespace(ns);
        let mut sub_opts = opts.clone();
        sub_opts.limit = limit;
        sub_opts.continue_token = inner_tok.clone();
        match fetch_raw_paged_json(
            project,
            &sub_cfg,
            (api_prefix, plural, false),
            &sub_opts,
            req_timeout,
        ) {
            Ok((stdout, used_tok, _, _, _)) => {
                if let Some((items, cont, rem, rv)) = parse_fn(&stdout) {
                    any_ok = true;
                    if !rv.is_empty() {
                        namespace_versions.insert(ns.clone(), rv);
                    }
                    // If `inner_tok` expired and `fetch_raw_paged_json` reset `used_tok` to empty, keep going cleanly.
                    let _ = used_tok;
                    collected.extend(items);
                    if !cont.is_empty() {
                        next_continue = format!("mns:{idx}:{cont}");
                        if let Some(r) = rem {
                            rem_total += r;
                            any_rem = true;
                        }
                        idx = ns_len;
                    } else {
                        idx += 1;
                    }
                }
            }
            Err(RawPageOutcome::Failed(err)) => {
                last_err = Some(err);
                idx += 1;
            }
            Err(_) => {
                return RawPageOutcome::Unsupported;
            }
        }
        inner_tok.clear();
    }

    // 2. Walk remaining selected namespaces in bounded batches of up to 4.
    while idx < ns_len && next_continue.is_empty() {
        let room = max_collect_cap.saturating_sub(collected.len()) as u32;
        if room == 0 {
            next_continue = format!("mns:{idx}:");
            any_rem = true;
            break;
        }
        let batch_end = (idx + 4).min(ns_len);
        let batch_slice = &namespaces[idx..batch_end];
        let req_per_ns = if eager_watch_drain {
            room.clamp(10, 500)
        } else if collected.is_empty() {
            limit
        } else {
            room.max(10).min(limit)
        };
        let uris: Vec<String> = batch_slice
            .iter()
            .map(|ns| {
                let sub_cfg = cfg.with_single_namespace(ns);
                build_paged_raw_uri(api_prefix, plural, false, &sub_cfg, req_per_ns, "")
            })
            .collect();
        let allow_parallel = req_per_ns <= 100 && batch_slice.len() > 1;
        let outputs = run_raw_uri_batch(project, cfg, &uris, allow_parallel, req_timeout);

        let mut stopped_in_batch = false;
        for (offset, (ns, out_res)) in batch_slice.iter().zip(outputs).enumerate() {
            let ns_idx = idx + offset;
            match out_res {
                Ok(out) if out.status == Some(0) => {
                    if !out.stdout.trim_start().starts_with('{') {
                        return RawPageOutcome::Unsupported;
                    }
                    let Some((mut items, mut cont, mut rem, mut rv)) = parse_fn(&out.stdout) else {
                        return RawPageOutcome::Unsupported;
                    };
                    any_ok = true;
                    if !rv.is_empty() {
                        namespace_versions.insert(ns.clone(), rv.clone());
                    }
                    if stopped_in_batch {
                        rem_total += items.len() as u64 + rem.unwrap_or(0);
                        any_rem = true;
                        continue;
                    }
                    // When <= 5 namespaces are selected and combined total stays <= 500, drain any
                    // continuation inside `ns` so all <= 500 items load up front with live watch.
                    if eager_watch_drain
                        && !cont.is_empty()
                        && (collected.len() as u64 + items.len() as u64 + rem.unwrap_or(0))
                            <= WATCH_ELIGIBLE_MAX_ITEMS
                    {
                        let sub_cfg = cfg.with_single_namespace(ns);
                        for _ in 0..4 {
                            if cont.is_empty() {
                                break;
                            }
                            let drain_opts = FetchOptions {
                                limit: 250,
                                continue_token: cont.clone(),
                                ..opts.clone()
                            };
                            let Ok((d_out, _, _, _, _)) = fetch_raw_paged_json(
                                project,
                                &sub_cfg,
                                (api_prefix, plural, false),
                                &drain_opts,
                                req_timeout,
                            ) else {
                                break;
                            };
                            let Some((d_items, d_cont, d_rem, d_rv)) = parse_fn(&d_out) else {
                                break;
                            };
                            if (collected.len() + items.len() + d_items.len()) as u64
                                + d_rem.unwrap_or(0)
                                > WATCH_ELIGIBLE_MAX_ITEMS
                            {
                                break;
                            }
                            items.extend(d_items);
                            cont = d_cont;
                            rem = d_rem;
                            if !d_rv.is_empty() {
                                rv = d_rv;
                                namespace_versions.insert(ns.clone(), rv.clone());
                            }
                        }
                    }

                    let est_after = collected.len() as u64 + items.len() as u64 + rem.unwrap_or(0);
                    let active_cap = if eager_watch_drain && est_after <= WATCH_ELIGIBLE_MAX_ITEMS {
                        WATCH_ELIGIBLE_MAX_ITEMS as usize
                    } else {
                        limit as usize
                    };
                    if !collected.is_empty() && collected.len() + items.len() > active_cap {
                        next_continue = format!("mns:{ns_idx}:");
                        rem_total += items.len() as u64 + rem.unwrap_or(0);
                        any_rem = true;
                        stopped_in_batch = true;
                        continue;
                    }
                    collected.extend(items);
                    if !cont.is_empty() {
                        next_continue = format!("mns:{ns_idx}:{cont}");
                        rem_total += rem.unwrap_or(0);
                        any_rem = true;
                        stopped_in_batch = true;
                    }
                }
                Ok(out) => {
                    if out.stderr.trim().is_empty() && out.stdout.trim().is_empty() {
                        return RawPageOutcome::Unsupported;
                    }
                    last_err = Some(first_line_or(
                        &out.stderr,
                        "kubectl raw list request failed",
                    ));
                }
                Err(e) => {
                    last_err = Some(e.to_string());
                }
            }
        }
        idx = batch_end;
        let stop_cap = if eager_watch_drain && collected.len() <= WATCH_ELIGIBLE_MAX_ITEMS as usize
        {
            WATCH_ELIGIBLE_MAX_ITEMS as usize
        } else {
            limit as usize
        };
        if !stopped_in_batch && collected.len() >= stop_cap && idx < ns_len {
            next_continue = format!("mns:{idx}:");
            any_rem = true;
            break;
        }
    }

    if !any_ok {
        return match last_err {
            Some(err) => RawPageOutcome::Failed(err),
            None => RawPageOutcome::Unsupported,
        };
    }

    let summary_rv = if namespace_versions.len() == ns_len
        && namespace_versions.values().all(|rv| !rv.is_empty())
    {
        "multi".to_owned()
    } else {
        String::new()
    };

    RawPageOutcome::Page(Box::new(FetchedPage {
        items: collected,
        request_token: req_tok,
        continue_token: next_continue.clone(),
        remaining_count: if !next_continue.is_empty() || any_rem {
            Some(rem_total)
        } else {
            None
        },
        resource_version: summary_rv,
        namespace_versions,
        page_index: page_idx,
        append,
        used_limit: limit,
    }))
}

fn fetch_raw_paged_json(
    project: &Project,
    cfg: &KubeConfigState,
    endpoint: (&str, &str, bool),
    opts: &FetchOptions,
    req_timeout: Option<u32>,
) -> Result<(String, String, u32, bool, u32), RawPageOutcome<ClusterResource>> {
    let (api_prefix, plural, cluster_scoped) = endpoint;
    let mut limit = opts.limit.clamp(10, 500);
    let mut req_tok = opts.continue_token.trim().to_owned();
    let mut page_idx = opts.page_index.max(1);
    let mut append = opts.append;

    for attempt in 0..2 {
        let uri = build_paged_raw_uri(api_prefix, plural, cluster_scoped, cfg, limit, &req_tok);
        let mut args = cfg.base_args(false);
        args.extend(["get".into(), "--raw".into(), uri]);
        match run_kubectl_timed(project, &args, None, req_timeout) {
            Ok(out) if out.status == Some(0) => {
                if !out.stdout.trim_start().starts_with('{') {
                    return Err(RawPageOutcome::Unsupported);
                }
                return Ok((out.stdout, req_tok, page_idx, append, limit));
            }
            Ok(out) => {
                let combined = format!("{}\n{}", out.stderr, out.stdout);
                // Expired Kubernetes `continue` token (`410 Gone` / `ResourceExpired`): reset cleanly to page 1.
                if attempt == 0
                    && !req_tok.is_empty()
                    && (combined.contains("continue")
                        || combined.contains("410")
                        || combined.contains("ResourceExpired")
                        || combined.contains("expired"))
                {
                    req_tok.clear();
                    page_idx = 1;
                    append = false;
                    continue;
                }
                if out.stderr.trim().is_empty() && out.stdout.trim().is_empty() {
                    return Err(RawPageOutcome::Unsupported);
                }
                return Err(RawPageOutcome::Failed(first_line_or(
                    &out.stderr,
                    "kubectl raw list request failed",
                )));
            }
            Err(e) => {
                let msg = e.to_string();
                if attempt == 0 && limit > 25 && msg.contains("exceeded") {
                    limit = 25;
                    continue;
                }
                return Err(RawPageOutcome::Failed(msg));
            }
        }
    }
    Err(RawPageOutcome::Unsupported)
}

fn fetch_raw_paged_single_or_cluster<T, F>(
    project: &Project,
    cfg: &KubeConfigState,
    endpoint: (&str, &str, bool),
    opts: &FetchOptions,
    req_timeout: Option<u32>,
    mut parse_fn: F,
) -> RawPageOutcome<T>
where
    F: FnMut(&str) -> Option<(Vec<T>, String, Option<u64>, String)>,
{
    match fetch_raw_paged_json(project, cfg, endpoint, opts, req_timeout) {
        Ok((stdout, req_tok, page_idx, append, used_limit)) => {
            let Some((mut items, mut cont, mut rem, mut rv)) = parse_fn(&stdout) else {
                return RawPageOutcome::Unsupported;
            };
            // If the entire scope has <= WATCH_ELIGIBLE_MAX_ITEMS (500) items, drain remaining pages
            // immediately so 100% of items are in memory and live `watch=true` streaming activates.
            if req_tok.is_empty()
                && !append
                && !cont.is_empty()
                && (items.len() as u64 + rem.unwrap_or(0)) <= WATCH_ELIGIBLE_MAX_ITEMS
            {
                for _ in 0..5 {
                    if cont.is_empty() {
                        break;
                    }
                    let drain_opts = FetchOptions {
                        limit: 250,
                        continue_token: cont.clone(),
                        ..opts.clone()
                    };
                    let Ok((d_out, _, _, _, _)) =
                        fetch_raw_paged_json(project, cfg, endpoint, &drain_opts, req_timeout)
                    else {
                        break;
                    };
                    let Some((d_items, d_cont, d_rem, d_rv)) = parse_fn(&d_out) else {
                        break;
                    };
                    if (items.len() + d_items.len()) as u64 + d_rem.unwrap_or(0)
                        > WATCH_ELIGIBLE_MAX_ITEMS
                    {
                        break;
                    }
                    items.extend(d_items);
                    cont = d_cont;
                    rem = d_rem;
                    if !d_rv.is_empty() {
                        rv = d_rv;
                    }
                }
                if cont.is_empty() {
                    rem = None;
                }
            }
            RawPageOutcome::Page(Box::new(FetchedPage {
                items,
                request_token: req_tok,
                continue_token: cont,
                remaining_count: rem,
                resource_version: rv,
                namespace_versions: BTreeMap::new(),
                page_index: page_idx,
                append,
                used_limit,
            }))
        }
        Err(RawPageOutcome::Unsupported) => RawPageOutcome::Unsupported,
        Err(RawPageOutcome::Failed(msg)) => RawPageOutcome::Failed(msg),
        Err(RawPageOutcome::Page(_)) => RawPageOutcome::Unsupported,
    }
}

fn fetch_raw_paged_resources(
    project: &Project,
    cfg: &KubeConfigState,
    category: &str,
    endpoint: (&str, &str, bool),
    opts: &FetchOptions,
    req_timeout: Option<u32>,
) -> RawPageOutcome<ClusterResource> {
    if !endpoint.2 && cfg.is_multi_namespace() {
        return fetch_raw_paged_multi_ns(
            project,
            cfg,
            (endpoint.0, endpoint.1),
            opts,
            req_timeout,
            |stdout| parse_paged_resource_list_json_with_rv(stdout, Some(category)),
        );
    }
    fetch_raw_paged_single_or_cluster(project, cfg, endpoint, opts, req_timeout, |stdout| {
        parse_paged_resource_list_json_with_rv(stdout, Some(category))
    })
}

fn fetch_raw_paged_events(
    project: &Project,
    cfg: &KubeConfigState,
    opts: &FetchOptions,
    req_timeout: Option<u32>,
) -> RawPageOutcome<ClusterEvent> {
    if cfg.is_multi_namespace() {
        return fetch_raw_paged_multi_ns(
            project,
            cfg,
            ("/api/v1", "events"),
            opts,
            req_timeout,
            |stdout| parse_paged_events_json_with_rv(stdout, None),
        );
    }
    fetch_raw_paged_single_or_cluster(
        project,
        cfg,
        ("/api/v1", "events", false),
        opts,
        req_timeout,
        |stdout| parse_paged_events_json_with_rv(stdout, None),
    )
}

fn compute_page_watch_supported(
    resource_version: &str,
    namespace_versions: &BTreeMap<String, String>,
    page_index: u32,
    continue_token: &str,
    est_total: u64,
    append: bool,
) -> bool {
    if !continue_token.is_empty() || est_total > WATCH_ELIGIBLE_MAX_ITEMS {
        return false;
    }
    if !append && page_index > 1 {
        return false;
    }
    if namespace_versions.is_empty() {
        !resource_version.is_empty()
    } else {
        !resource_version.is_empty()
            && namespace_versions.len() <= MAX_MULTI_NAMESPACE_WATCHES
            && namespace_versions.values().all(|rv| !rv.is_empty())
    }
}

fn apply_resource_page_to_snapshot(
    snap: &mut ClusterSnapshot,
    category: &str,
    page: FetchedPage<ClusterResource>,
) {
    let FetchedPage {
        items,
        request_token,
        continue_token,
        remaining_count,
        resource_version,
        namespace_versions,
        page_index,
        append,
        used_limit,
    } = page;
    let page_len = items.len() as u64;
    let est_total = if request_token.is_empty() {
        let total = page_len + remaining_count.unwrap_or(0);
        snap.overview_counts.insert(category.to_owned(), total);
        total
    } else {
        snap.overview_counts
            .get(category)
            .copied()
            .unwrap_or(page_len + remaining_count.unwrap_or(0))
    };
    if append {
        let existing_keys: std::collections::BTreeSet<(String, String)> = snap
            .resources
            .iter()
            .filter(|r| r.kind == category)
            .map(|r| (r.namespace.clone(), r.name.clone()))
            .collect();
        let mut current_cat_cnt = existing_keys.len();
        for item in items {
            if current_cat_cnt >= MAX_APPENDED_ITEMS {
                break;
            }
            if !existing_keys.contains(&(item.namespace.clone(), item.name.clone())) {
                snap.resources.push(item);
                current_cat_cnt += 1;
            }
        }
        let watch_supported = compute_page_watch_supported(
            &resource_version,
            &namespace_versions,
            page_index,
            &continue_token,
            est_total,
            true,
        );
        snap.page_info.insert(
            category.to_owned(),
            CategoryPageInfo {
                category: category.to_owned(),
                limit: used_limit,
                request_token,
                continue_token,
                remaining_count,
                loaded_count: current_cat_cnt as u64,
                page_index,
                mode: "append".into(),
                resource_version,
                namespace_versions,
                watch_supported,
            },
        );
    } else {
        snap.resources.retain(|r| {
            r.kind == "CustomResourceDefinition" && category != "CustomResourceDefinition"
        });
        snap.resources.extend(items);
        let loaded = snap.resources.iter().filter(|r| r.kind == category).count() as u64;
        let watch_supported = compute_page_watch_supported(
            &resource_version,
            &namespace_versions,
            page_index,
            &continue_token,
            est_total,
            false,
        );
        snap.page_info.insert(
            category.to_owned(),
            CategoryPageInfo {
                category: category.to_owned(),
                limit: used_limit,
                request_token,
                continue_token,
                remaining_count,
                loaded_count: loaded,
                page_index,
                mode: "replace".into(),
                resource_version,
                namespace_versions,
                watch_supported,
            },
        );
    }
}

/// Fetches a single category on demand (used for instant ~200ms hotbar context switching and lazy sidebar tabs).
pub fn fetch_category_into_snapshot(
    project: &Project,
    cfg: &KubeConfigState,
    snap: &mut ClusterSnapshot,
    category: &str,
) {
    fetch_category_with_options(project, cfg, snap, category, FetchOptions::default());
}

pub fn fetch_category_with_options(
    project: &Project,
    cfg: &KubeConfigState,
    snap: &mut ClusterSnapshot,
    category: &str,
    opts: FetchOptions,
) {
    let req_timeout = if opts.is_background_poll {
        Some(3)
    } else {
        Some(15)
    };
    let started = std::time::Instant::now();
    match category {
        "CustomResourceDefinition" => {
            fetch_crds_into_snapshot(project, cfg, snap);
            let loaded = snap
                .resources
                .iter()
                .filter(|r| r.kind == "CustomResourceDefinition")
                .count() as u64;
            snap.overview_counts
                .insert("CustomResourceDefinition".into(), loaded);
            snap.page_info.insert(
                "CustomResourceDefinition".into(),
                CategoryPageInfo {
                    category: "CustomResourceDefinition".into(),
                    limit: loaded.max(DEFAULT_PAGE_LIMIT as u64) as u32,
                    request_token: String::new(),
                    continue_token: String::new(),
                    remaining_count: None,
                    loaded_count: loaded,
                    page_index: 1,
                    mode: "replace".into(),
                    resource_version: String::new(),
                    namespace_versions: BTreeMap::new(),
                    watch_supported: false,
                },
            );
        }
        "HelmRelease" => fetch_helm_into_snapshot(project, cfg, snap),
        "ClusterOverview" | "WorkloadsOverview" => {
            fetch_workloads_into_snapshot(project, cfg, snap);
            fetch_secrets_into_snapshot(project, cfg, snap);
        }
        "ClusterCatalog" | "Workspace" => {}
        "Event" => match fetch_raw_paged_events(project, cfg, &opts, req_timeout) {
            RawPageOutcome::Page(page) => {
                snap.connected = true;
                snap.error = None;
                let page_len = page.items.len() as u64;
                let est_total = page_len + page.remaining_count.unwrap_or(0);
                if page.request_token.is_empty() {
                    snap.overview_counts.insert("Event".into(), est_total);
                }
                if page.append {
                    let room = MAX_APPENDED_ITEMS.saturating_sub(snap.events.len());
                    snap.events.extend(page.items.into_iter().take(room));
                } else {
                    snap.events = page.items;
                }
                let loaded = snap.events.len() as u64;
                let watch_supported = compute_page_watch_supported(
                    &page.resource_version,
                    &page.namespace_versions,
                    page.page_index,
                    &page.continue_token,
                    est_total,
                    page.append,
                );
                snap.page_info.insert(
                    "Event".into(),
                    CategoryPageInfo {
                        category: "Event".into(),
                        limit: page.used_limit,
                        request_token: page.request_token,
                        continue_token: page.continue_token,
                        remaining_count: page.remaining_count,
                        loaded_count: loaded,
                        page_index: page.page_index,
                        mode: if page.append {
                            "append".into()
                        } else {
                            "replace".into()
                        },
                        resource_version: page.resource_version,
                        namespace_versions: page.namespace_versions,
                        watch_supported,
                    },
                );
            }
            RawPageOutcome::Unsupported => fetch_events_into_snapshot(project, cfg, snap),
            RawPageOutcome::Failed(err) => {
                if !opts.is_background_poll {
                    fetch_events_into_snapshot(project, cfg, snap);
                    if snap.events.is_empty() {
                        snap.error = Some(err);
                    }
                }
            }
        },
        kind if kind.starts_with("CR:") => {
            let crd_name = kind.strip_prefix("CR:").unwrap_or(kind);
            if !snap
                .resources
                .iter()
                .any(|r| r.kind == "CustomResourceDefinition")
            {
                fetch_crds_into_snapshot(project, cfg, snap);
            }
            if let Some((api_prefix, plural, is_cluster_scoped)) =
                resolve_crd_api_endpoint(project, cfg, snap, crd_name, req_timeout)
            {
                match fetch_raw_paged_resources(
                    project,
                    cfg,
                    kind,
                    (&api_prefix, &plural, is_cluster_scoped),
                    &opts,
                    req_timeout,
                ) {
                    RawPageOutcome::Page(page) => {
                        snap.connected = true;
                        snap.error = None;
                        apply_resource_page_to_snapshot(snap, kind, *page);
                        return;
                    }
                    RawPageOutcome::Unsupported | RawPageOutcome::Failed(_) => {}
                }
            }
            let is_cluster_scoped = snap
                .resources
                .iter()
                .find(|r| r.kind == "CustomResourceDefinition" && r.name == crd_name)
                .is_some_and(|r| r.status.eq_ignore_ascii_case("Cluster"));
            let mut cr_args = cfg.base_args(!is_cluster_scoped);
            cr_args.extend(["get".into(), crd_name.to_owned()]);
            if !is_cluster_scoped {
                cfg.push_scope_flags(&mut cr_args);
            }
            if cfg.all_namespaces {
                cr_args.push("--no-headers".into());
            } else {
                cr_args.extend([
                    "-o".into(),
                    CR_INSTANCE_COLUMNS_SPEC.into(),
                    "--no-headers".into(),
                ]);
            }
            if let Ok(out) = run_kubectl_timed(project, &cr_args, None, req_timeout)
                && out.status == Some(0)
            {
                snap.connected = true;
                snap.error = None;
                snap.resources.retain(|r| r.kind != kind);
                snap.resources
                    .extend(parse_cr_instance_lines(&out.stdout, crd_name));
            }
        }
        kind => {
            let prev_pod_metrics: BTreeMap<(String, String), (String, String)> = if kind == "Pod" {
                snap.resources
                    .iter()
                    .filter(|r| r.kind == "Pod" && (!r.cpu.is_empty() || !r.memory.is_empty()))
                    .map(|r| {
                        (
                            (r.namespace.clone(), r.name.clone()),
                            (r.cpu.clone(), r.memory.clone()),
                        )
                    })
                    .collect()
            } else {
                BTreeMap::new()
            };
            let prev_node_metrics: BTreeMap<String, (String, String)> = if kind == "Node" {
                snap.resources
                    .iter()
                    .filter(|r| r.kind == "Node" && (!r.cpu.is_empty() || !r.memory.is_empty()))
                    .map(|r| (r.name.clone(), (r.cpu.clone(), r.memory.clone())))
                    .collect()
            } else {
                BTreeMap::new()
            };

            if let Some(endpoint) = k8s_api_endpoint_info(kind) {
                match fetch_raw_paged_resources(project, cfg, kind, endpoint, &opts, req_timeout) {
                    RawPageOutcome::Page(mut page) => {
                        snap.connected = true;
                        snap.error = None;
                        let est_total =
                            (page.items.len() as u64) + page.remaining_count.unwrap_or(0);

                        if kind == "Pod" {
                            let finished_debuggers: Vec<(String, String)> = page
                                .items
                                .iter()
                                .filter(|r| {
                                    (r.name.starts_with("node-debugger-")
                                        || r.name.starts_with("node-dbg-watch-"))
                                        && matches!(
                                            r.status.as_str(),
                                            "Succeeded" | "Completed" | "Failed"
                                        )
                                })
                                .map(|r| (r.namespace.clone(), r.name.clone()))
                                .collect();
                            if !finished_debuggers.is_empty() && !opts.is_background_poll {
                                for (ns, name) in &finished_debuggers {
                                    let _ = delete_cluster_resource(project, cfg, "Pod", ns, name);
                                }
                            }
                            page.items.retain(|r| {
                                !(r.name.starts_with("node-dbg-watch-")
                                    || (r.name.starts_with("node-debugger-")
                                        && matches!(
                                            r.status.as_str(),
                                            "Succeeded" | "Completed" | "Failed"
                                        )))
                            });
                        }

                        apply_resource_page_to_snapshot(snap, kind, *page);

                        if kind == "Pod" {
                            let allow_metrics = opts.include_metrics
                                && est_total <= 300
                                && (!opts.is_background_poll
                                    || started.elapsed() < std::time::Duration::from_millis(1200));
                            if allow_metrics {
                                enrich_pod_metrics(project, cfg, snap);
                            } else if !prev_pod_metrics.is_empty() {
                                for r in snap.resources.iter_mut().filter(|r| r.kind == "Pod") {
                                    if let Some((cpu, mem)) =
                                        prev_pod_metrics.get(&(r.namespace.clone(), r.name.clone()))
                                    {
                                        r.cpu = cpu.clone();
                                        r.memory = mem.clone();
                                    }
                                }
                            }
                        } else if kind == "Node" {
                            let allow_node_top = opts.include_metrics
                                && est_total <= 80
                                && (!opts.is_background_poll
                                    || started.elapsed() < std::time::Duration::from_millis(1200));
                            if allow_node_top {
                                let mut top_args = cfg.base_args(false);
                                top_args.extend([
                                    "top".into(),
                                    "nodes".into(),
                                    "--no-headers".into(),
                                ]);
                                if let Ok(top_out) =
                                    run_kubectl_timed(project, &top_args, None, Some(3))
                                    && top_out.status == Some(0)
                                {
                                    let mut top_map: BTreeMap<String, (f64, f64)> = BTreeMap::new();
                                    for line in top_out.stdout.lines() {
                                        let cols: Vec<&str> = line.split_whitespace().collect();
                                        if cols.len() >= 4 {
                                            top_map.insert(
                                                cols[0].to_owned(),
                                                (parse_kube_cores(cols[1]), parse_kube_gi(cols[3])),
                                            );
                                        }
                                    }
                                    for r in snap.resources.iter_mut().filter(|r| r.kind == "Node")
                                    {
                                        if let Some(&(u_cpu, u_mem)) = top_map.get(&r.name) {
                                            if let Some((_, a_cpu)) = r.cpu.split_once('/') {
                                                r.cpu = format!("{u_cpu:.2}/{a_cpu}");
                                            }
                                            if let Some((_, a_mem)) = r.memory.split_once('/') {
                                                r.memory = format!("{u_mem:.1}Gi/{a_mem}");
                                            }
                                        }
                                    }
                                }
                            } else if !prev_node_metrics.is_empty() {
                                for r in snap.resources.iter_mut().filter(|r| r.kind == "Node") {
                                    if let Some((p_cpu, p_mem)) = prev_node_metrics.get(&r.name) {
                                        r.cpu = p_cpu.clone();
                                        r.memory = p_mem.clone();
                                    }
                                }
                            }
                        }
                        return;
                    }
                    RawPageOutcome::Unsupported => {}
                    RawPageOutcome::Failed(err) => {
                        if opts.is_background_poll {
                            snap.error = Some(err);
                            return;
                        }
                    }
                }
            }

            if kind == "Namespace" {
                fetch_namespaces_table_fallback(project, cfg, snap);
                return;
            }
            if kind == "Node" {
                fetch_nodes_with_options(project, cfg, snap, opts);
                return;
            }
            if kind == "Secret" {
                fetch_secrets_into_snapshot(project, cfg, snap);
                return;
            }

            let (kubectl_type, cluster_scoped, _) = kubectl_resource_info(kind);
            let use_server_table = cfg.all_namespaces && !cluster_scoped;
            let mut res_args = cfg.base_args(!cluster_scoped);
            res_args.extend(["get".into(), kubectl_type.into()]);
            if !cluster_scoped {
                cfg.push_scope_flags(&mut res_args);
            }
            if use_server_table {
                if kind == "Pod" {
                    res_args.extend(["-o".into(), "wide".into(), "--no-headers".into()]);
                } else {
                    res_args.push("--no-headers".into());
                }
            } else {
                res_args.extend(["-o".into(), "json".into(), "--chunk-size=500".into()]);
            }
            match run_kubectl_timed(project, &res_args, None, req_timeout) {
                Ok(out) if out.status == Some(0) => {
                    snap.connected = true;
                    snap.error = None;
                    snap.resources.retain(|r| r.kind != kind);
                    if out.stdout.trim_start().starts_with('{') {
                        snap.resources.extend(parse_resource_list_json(&out.stdout));
                    } else if use_server_table {
                        snap.resources.extend(parse_server_table_resources(
                            kind,
                            &out.stdout,
                            cfg.all_namespaces,
                            &cfg.namespace,
                        ));
                    } else {
                        snap.resources
                            .extend(parse_custom_columns_resources(&out.stdout));
                    }
                    if kind == "Pod" {
                        let finished_debuggers: Vec<(String, String)> = snap
                            .resources
                            .iter()
                            .filter(|r| {
                                r.kind == "Pod"
                                    && (r.name.starts_with("node-debugger-")
                                        || r.name.starts_with("node-dbg-watch-"))
                                    && matches!(
                                        r.status.as_str(),
                                        "Succeeded" | "Completed" | "Failed"
                                    )
                            })
                            .map(|r| (r.namespace.clone(), r.name.clone()))
                            .collect();
                        if !finished_debuggers.is_empty() && !opts.is_background_poll {
                            for (ns, name) in &finished_debuggers {
                                let _ = delete_cluster_resource(project, cfg, "Pod", ns, name);
                            }
                        }
                        snap.resources.retain(|r| {
                            !(r.kind == "Pod"
                                && (r.name.starts_with("node-dbg-watch-")
                                    || (r.name.starts_with("node-debugger-")
                                        && matches!(
                                            r.status.as_str(),
                                            "Succeeded" | "Completed" | "Failed"
                                        ))))
                        });
                        let pod_cnt = snap.resources.iter().filter(|r| r.kind == "Pod").count();
                        let allow_metrics = opts.include_metrics
                            && pod_cnt <= 300
                            && (!opts.is_background_poll
                                || started.elapsed() < std::time::Duration::from_millis(1200));
                        if allow_metrics {
                            enrich_pod_metrics(project, cfg, snap);
                        } else if !prev_pod_metrics.is_empty() {
                            for r in snap.resources.iter_mut().filter(|r| r.kind == "Pod") {
                                if let Some((cpu, mem)) =
                                    prev_pod_metrics.get(&(r.namespace.clone(), r.name.clone()))
                                {
                                    r.cpu = cpu.clone();
                                    r.memory = mem.clone();
                                }
                            }
                        }
                    }
                }
                Ok(out) => {
                    snap.error = Some(first_line_or(&out.stderr, "kubectl get resources failed"));
                }
                Err(e) => {
                    if !use_server_table && !cluster_scoped && !opts.is_background_poll {
                        let mut fallback_args = cfg.base_args(true);
                        fallback_args.extend(["get".into(), kubectl_type.into()]);
                        if kind == "Pod" {
                            fallback_args.extend([
                                "-o".into(),
                                "wide".into(),
                                "--no-headers".into(),
                            ]);
                        } else {
                            fallback_args.push("--no-headers".into());
                        }
                        if let Ok(out) = run_kubectl_timed(project, &fallback_args, None, Some(15))
                            && out.status == Some(0)
                        {
                            snap.connected = true;
                            snap.error = None;
                            snap.resources.retain(|r| r.kind != kind);
                            snap.resources.extend(parse_server_table_resources(
                                kind,
                                &out.stdout,
                                false,
                                &cfg.namespace,
                            ));
                            return;
                        }
                    }
                    snap.error = Some(e.to_string());
                }
            }
        }
    }
}

/// Fetches a `ClusterSnapshot` using `kubectl`.
/// Heavy cluster-wide categories (`Node`, `CustomResourceDefinition`, `Event`, `HelmRelease`) are loaded on demand when selected.
pub fn fetch_cluster_snapshot(project: &Project, cfg: &mut KubeConfigState) -> ClusterSnapshot {
    let _task = host::begin_task("Kubernetes: Refreshing cluster", true);
    let mut snap = ClusterSnapshot {
        connected: false,
        error: None,
        current_context: cfg.context.clone(),
        contexts: Vec::new(),
        cluster_catalog: Vec::new(),
        current_namespace: cfg.namespace.clone(),
        all_namespaces: cfg.all_namespaces,
        selected_namespaces: cfg.active_namespaces(),
        namespaces: vec!["default".into(), "kube-system".into()],
        resources: Vec::new(),
        events: Vec::new(),
        helm_releases: Vec::new(),
        ..Default::default()
    };

    match fetch_kubeconfig_overview(project, cfg) {
        Ok((current_ctx, contexts, ctx_ns, catalog)) => {
            if cfg.context.is_empty() && !current_ctx.is_empty() {
                cfg.context = current_ctx.clone();
            }
            if cfg.namespace == "default"
                && cfg.selected_namespaces.len() <= 1
                && let Some(ns) = ctx_ns
                && !ns.is_empty()
            {
                cfg.set_namespaces([ns.as_str()]);
                cfg.persist_scope();
            }
            snap.current_context = if cfg.context.is_empty() {
                current_ctx
            } else {
                cfg.context.clone()
            };
            snap.contexts = contexts;
            snap.cluster_catalog = catalog;
            snap.current_namespace = cfg.namespace.clone();
            snap.selected_namespaces = cfg.active_namespaces();
        }
        Err(e) => {
            snap.error = Some(e.to_string());
            return snap;
        }
    }

    if snap.current_context.is_empty() {
        snap.error = Some("No Kubernetes contexts found in kubeconfig (~/.kube/config)".into());
        return snap;
    }

    fetch_namespaces_into_snapshot(project, cfg, &mut snap);
    enforce_large_cluster_scope(cfg, &mut snap);
    fetch_workloads_into_snapshot(project, cfg, &mut snap);
    fetch_secrets_into_snapshot(project, cfg, &mut snap);

    snap
}

/// Fetches Pod logs (`kubectl logs`).
pub fn fetch_pod_logs(
    project: &Project,
    cfg: &KubeConfigState,
    namespace: &str,
    pod: &str,
    container: Option<&str>,
    previous: bool,
) -> Result<String, Error> {
    let mut args = cfg.target_args(Some(namespace));
    args.push("logs".into());
    args.push(pod.to_owned());
    if let Some(c) = container.filter(|c| !c.is_empty()) {
        args.push("-c".into());
        args.push(c.to_owned());
    }
    if previous {
        args.push("--previous".into());
    }
    args.push(format!("--tail={}", cfg.tail_lines));
    args.push("--timestamps=true".into());
    let out = run_kubectl(project, &args, None)?;
    if out.status != Some(0) {
        return Err(Error::Failed(first_line_or(
            &out.stderr,
            "kubectl logs failed",
        )));
    }
    Ok(out.stdout)
}

/// Runs `kubectl describe <kind> <name>` or `kubectl get <kind> <name> -o yaml`.
pub fn inspect_cluster_resource(
    project: &Project,
    cfg: &KubeConfigState,
    kind: &str,
    namespace: &str,
    name: &str,
    as_yaml: bool,
) -> Result<String, Error> {
    if kind == "HelmRelease" {
        return fetch_helm_release_detail(
            project,
            cfg,
            namespace,
            name,
            if as_yaml { "manifest" } else { "all" },
        );
    }
    let target_type = kind
        .strip_prefix("CR:")
        .unwrap_or(kind)
        .to_ascii_lowercase();
    let (_, cluster_scoped, _) = if kind.starts_with("CR:") {
        ("cr", namespace.is_empty(), false)
    } else {
        kubectl_resource_info(kind)
    };
    let mut args = if cluster_scoped || namespace.is_empty() {
        cfg.base_args(false)
    } else {
        cfg.target_args(Some(namespace))
    };
    if as_yaml {
        args.extend([
            "get".into(),
            target_type,
            name.to_owned(),
            "-o".into(),
            "yaml".into(),
        ]);
    } else {
        args.extend(["describe".into(), target_type, name.to_owned()]);
    }
    let out = run_kubectl(project, &args, None)?;
    if out.status != Some(0) {
        return Err(Error::Failed(first_line_or(
            &out.stderr,
            "kubectl resource query failed",
        )));
    }
    Ok(out.stdout)
}

/// Runs `kubectl get <kind> <name> -o json` to populate the Freelens structured Detail Drawer.
pub fn inspect_cluster_resource_json(
    project: &Project,
    cfg: &KubeConfigState,
    kind: &str,
    namespace: &str,
    name: &str,
) -> Result<String, Error> {
    if kind == "HelmRelease" {
        let mut base = Vec::new();
        if !cfg.kubeconfig.trim().is_empty() {
            base.push(format!("--kubeconfig={}", cfg.kubeconfig.trim()));
        }
        if !cfg.context.trim().is_empty() {
            base.push(format!("--kube-context={}", cfg.context.trim()));
        }
        if !namespace.trim().is_empty() {
            base.push("-n".into());
            base.push(namespace.trim().to_owned());
        }
        let mut status_args = base.clone();
        status_args.extend(["status".into(), name.to_owned(), "-o".into(), "json".into()]);
        let status_json: Value = project
            .run_process("helm", &status_args, None)
            .ok()
            .filter(|o| o.status == Some(0))
            .and_then(|o| serde_json::from_str(&o.stdout).ok())
            .unwrap_or_else(|| json!({}));

        let mut val_args = base;
        val_args.extend([
            "get".into(),
            "values".into(),
            name.to_owned(),
            "-o".into(),
            "json".into(),
        ]);
        let values_json: Value = project
            .run_process("helm", &val_args, None)
            .ok()
            .filter(|o| o.status == Some(0))
            .and_then(|o| serde_json::from_str(&o.stdout).ok())
            .unwrap_or_else(|| json!({}));

        let deployed_at = status_json
            .pointer("/info/last_deployed")
            .or_else(|| status_json.pointer("/info/first_deployed"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let notes = status_json
            .pointer("/info/notes")
            .and_then(Value::as_str)
            .unwrap_or("");
        let description = status_json
            .pointer("/info/description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let status_str = status_json
            .pointer("/info/status")
            .and_then(Value::as_str)
            .unwrap_or("deployed");
        let version = status_json
            .get("version")
            .and_then(Value::as_u64)
            .unwrap_or(1);

        let wrapped = json!({
            "kind": "HelmRelease",
            "metadata": {
                "name": name,
                "namespace": namespace,
                "creationTimestamp": deployed_at,
            },
            "helm": {
                "revision": version,
                "status": status_str,
                "description": description,
                "notes": notes,
                "values": values_json,
            }
        });
        return Ok(wrapped.to_string());
    }
    let target_type = kind
        .strip_prefix("CR:")
        .unwrap_or(kind)
        .to_ascii_lowercase();
    let (_, cluster_scoped, _) = if kind.starts_with("CR:") {
        ("cr", namespace.is_empty(), false)
    } else {
        kubectl_resource_info(kind)
    };
    let mut args = if cluster_scoped || namespace.is_empty() {
        cfg.base_args(false)
    } else {
        cfg.target_args(Some(namespace))
    };
    args.extend([
        "get".into(),
        target_type,
        name.to_owned(),
        "-o".into(),
        "json".into(),
    ]);
    let out = run_kubectl(project, &args, None)?;
    if out.status != Some(0) {
        return Err(Error::Failed(first_line_or(
            &out.stderr,
            "kubectl get resource json failed",
        )));
    }
    Ok(out.stdout)
}

/// Fetches related pods (with CPU/Memory metrics), related child resources, and involved events for the Drawer.
pub fn fetch_resource_extras(
    project: &Project,
    cfg: &KubeConfigState,
    kind: &str,
    namespace: &str,
    name: &str,
    raw_json: &str,
) -> Value {
    let mut related_pods: Vec<ClusterResource> = Vec::new();
    let mut related_resources: Vec<ClusterResource> = Vec::new();
    let mut events: Vec<ClusterEvent> = Vec::new();

    if kind == "Node" {
        let enc_node = url_encode_component(name);
        let mut raw_args = cfg.base_args(false);
        raw_args.extend([
            "get".into(),
            "--raw".into(),
            format!("/api/v1/pods?fieldSelector=spec.nodeName%3D{enc_node}&limit=250"),
        ]);
        if let Ok(out) = run_kubectl_timed(project, &raw_args, None, Some(5))
            && out.status == Some(0)
            && out.stdout.trim_start().starts_with('{')
            && let Some((pods, _, _)) = parse_paged_resource_list_json(&out.stdout, Some("Pod"))
        {
            related_pods = pods;
        } else {
            let mut p_args = cfg.base_args(false);
            p_args.extend([
                "get".into(),
                "pods".into(),
                "-A".into(),
                "--field-selector".into(),
                format!("spec.nodeName={name}"),
                "-o".into(),
                "wide".into(),
                "--no-headers".into(),
            ]);
            if let Ok(out) = run_kubectl_timed(project, &p_args, None, Some(5))
                && out.status == Some(0)
            {
                related_pods = parse_pods_wide_table(&out.stdout, true, "default");
            }
        }
    } else if matches!(
        kind,
        "Deployment" | "DaemonSet" | "StatefulSet" | "ReplicaSet" | "Job"
    ) && !namespace.is_empty()
    {
        if let Ok(parsed) = serde_json::from_str::<Value>(raw_json) {
            let selector = parsed
                .pointer("/spec/selector/matchLabels")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| v.as_str().map(|vs| format!("{k}={vs}")))
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            if !selector.is_empty() {
                let mut p_args = cfg.target_args(Some(namespace));
                p_args.extend([
                    "get".into(),
                    "pods".into(),
                    "-l".into(),
                    selector.clone(),
                    "-o".into(),
                    "json".into(),
                ]);
                if let Ok(out) = run_kubectl(project, &p_args, None)
                    && out.status == Some(0)
                {
                    let all_pods = parse_resource_list_json(&out.stdout);
                    if matches!(kind, "ReplicaSet" | "Job") {
                        let owned: Vec<ClusterResource> = all_pods
                            .iter()
                            .filter(|p| p.controlled_by_name == name)
                            .cloned()
                            .collect();
                        related_pods = if owned.is_empty() { all_pods } else { owned };
                    } else {
                        related_pods = all_pods;
                    }
                }
                if !related_pods.is_empty() {
                    let mut t_args = cfg.target_args(Some(namespace));
                    t_args.extend([
                        "top".into(),
                        "pods".into(),
                        "-l".into(),
                        selector.clone(),
                        "--no-headers".into(),
                    ]);
                    if let Ok(out) = run_kubectl(project, &t_args, None)
                        && out.status == Some(0)
                    {
                        let mut metrics: BTreeMap<String, (String, String)> = BTreeMap::new();
                        for line in out.stdout.lines() {
                            let cols: Vec<&str> = line.split_whitespace().collect();
                            if cols.len() >= 3 {
                                let pname = cols[0].to_owned();
                                let raw_cpu = cols[1];
                                let cpu = if let Some(m) = raw_cpu
                                    .strip_suffix('m')
                                    .and_then(|v| v.parse::<f64>().ok())
                                {
                                    format!("{:.3}", m / 1000.0)
                                } else {
                                    raw_cpu.to_owned()
                                };
                                metrics.insert(pname, (cpu, cols[2].to_owned()));
                            }
                        }
                        for pod in &mut related_pods {
                            if let Some((cpu, mem)) = metrics.get(&pod.name) {
                                pod.cpu = cpu.clone();
                                pod.memory = mem.clone();
                            }
                        }
                    }
                }
                if kind == "Deployment" {
                    let mut rs_args = cfg.target_args(Some(namespace));
                    rs_args.extend([
                        "get".into(),
                        "rs".into(),
                        "-l".into(),
                        selector,
                        "-o".into(),
                        "json".into(),
                    ]);
                    if let Ok(out) = run_kubectl(project, &rs_args, None)
                        && out.status == Some(0)
                    {
                        let all_rs = parse_resource_list_json(&out.stdout);
                        let owned: Vec<ClusterResource> = all_rs
                            .iter()
                            .filter(|r| r.controlled_by_name == name)
                            .cloned()
                            .collect();
                        related_resources = if owned.is_empty() { all_rs } else { owned };
                    }
                }
            }
        }
    } else if kind == "CronJob" && !namespace.is_empty() {
        let mut j_args = cfg.target_args(Some(namespace));
        j_args.extend(["get".into(), "jobs".into(), "-o".into(), "json".into()]);
        if let Ok(out) = run_kubectl(project, &j_args, None)
            && out.status == Some(0)
        {
            related_resources = parse_resource_list_json(&out.stdout)
                .into_iter()
                .filter(|r| r.controlled_by_name == name)
                .collect();
        }
    } else if kind == "StorageClass" {
        let mut pv_args = cfg.base_args(false);
        pv_args.extend([
            "get".into(),
            "--raw".into(),
            "/api/v1/persistentvolumes?limit=250".into(),
        ]);
        if let Ok(out) = run_kubectl_timed(project, &pv_args, None, Some(5))
            && out.status == Some(0)
            && out.stdout.trim_start().starts_with('{')
            && let Some((pvs, _, _)) =
                parse_paged_resource_list_json(&out.stdout, Some("PersistentVolume"))
        {
            related_resources = pvs
                .into_iter()
                .filter(|r| r.detail.contains(&format!("storageClass: {name}")))
                .collect();
        }
    } else if kind == "Namespace" {
        let mut rq_args = cfg.target_args(Some(name));
        rq_args.extend([
            "get".into(),
            "resourcequotas,limitranges".into(),
            "-o".into(),
            "json".into(),
        ]);
        if let Ok(out) = run_kubectl_timed(project, &rq_args, None, Some(4))
            && out.status == Some(0)
        {
            related_resources = parse_resource_list_json(&out.stdout);
        }
    } else if kind == "CustomResourceDefinition" {
        let mut cr_args = cfg.base_args(false);
        cr_args.extend([
            "get".into(),
            name.to_owned(),
            "-A".into(),
            "--no-headers".into(),
        ]);
        if let Ok(out) = run_kubectl_timed(project, &cr_args, None, Some(5))
            && out.status == Some(0)
        {
            related_resources = parse_cr_instance_lines(&out.stdout, name)
                .into_iter()
                .take(250)
                .collect();
        }
    } else if kind == "HelmRelease" {
        let mut rel_args = cfg.target_args(Some(namespace));
        rel_args.extend([
            "get".into(),
            "pods,deploy,sts,ds,svc,cm,sa,ing".into(),
            "-l".into(),
            format!("app.kubernetes.io/instance={name}"),
            "-o".into(),
            "json".into(),
        ]);
        if let Ok(out) = run_kubectl(project, &rel_args, None)
            && out.status == Some(0)
        {
            for r in parse_resource_list_json(&out.stdout) {
                if r.kind == "Pod" {
                    related_pods.push(r);
                } else {
                    related_resources.push(r);
                }
            }
            if !related_pods.is_empty() {
                let mut t_args = cfg.target_args(Some(namespace));
                t_args.extend([
                    "top".into(),
                    "pods".into(),
                    "-l".into(),
                    format!("app.kubernetes.io/instance={name}"),
                    "--no-headers".into(),
                ]);
                if let Ok(out) = run_kubectl(project, &t_args, None)
                    && out.status == Some(0)
                {
                    let mut metrics: BTreeMap<String, (String, String)> = BTreeMap::new();
                    for line in out.stdout.lines() {
                        let cols: Vec<&str> = line.split_whitespace().collect();
                        if cols.len() >= 3 {
                            let pname = cols[0].to_owned();
                            let raw_cpu = cols[1];
                            let cpu = if let Some(m) = raw_cpu
                                .strip_suffix('m')
                                .and_then(|v| v.parse::<f64>().ok())
                            {
                                format!("{:.3}", m / 1000.0)
                            } else {
                                raw_cpu.to_owned()
                            };
                            metrics.insert(pname, (cpu, cols[2].to_owned()));
                        }
                    }
                    for pod in &mut related_pods {
                        if let Some((cpu, mem)) = metrics.get(&pod.name) {
                            pod.cpu = cpu.clone();
                            pod.memory = mem.clone();
                        }
                    }
                }
            }
        }
    }

    if kind != "Event" {
        let (_, cluster_scoped, _) = kubectl_resource_info(kind);
        let mut ev_args = if cluster_scoped || namespace.is_empty() {
            cfg.base_args(false)
        } else {
            cfg.target_args(Some(namespace))
        };
        ev_args.extend([
            "get".into(),
            "events".into(),
            "--field-selector".into(),
            format!("involvedObject.name={name},involvedObject.kind={kind}"),
            "-o".into(),
            "json".into(),
        ]);
        if let Ok(out) = run_kubectl(project, &ev_args, None)
            && out.status == Some(0)
        {
            events = parse_events_json(&out.stdout);
        }
    }

    json!({
        "op": "resource-extras",
        "kind": kind,
        "namespace": namespace,
        "name": name,
        "relatedPods": related_pods,
        "relatedResources": related_resources,
        "events": events,
    })
}

/// Opens an interactive terminal following Pod logs (`kubectl logs -f`).
pub fn open_follow_logs_terminal(
    cfg: &KubeConfigState,
    namespace: &str,
    pod: &str,
    container: Option<&str>,
) -> Result<(), Error> {
    let mut args = cfg.target_args(Some(namespace));
    args.push("logs".into());
    args.push("-f".into());
    args.push(format!("--tail={}", cfg.tail_lines));
    args.push(pod.to_owned());
    if let Some(c) = container.filter(|c| !c.is_empty()) {
        args.push("-c".into());
        args.push(c.to_owned());
    }
    let title = match container.filter(|c| !c.is_empty()) {
        Some(c) => format!("k8s logs: {pod}/{c}"),
        None => format!("k8s logs: {pod}"),
    };
    host::open_terminal(&title, "kubectl", &args)
}

/// Opens an interactive shell inside a Pod container (`kubectl exec -it`).
pub fn open_exec_terminal(
    cfg: &KubeConfigState,
    namespace: &str,
    pod: &str,
    container: Option<&str>,
) -> Result<(), Error> {
    let mut args = cfg.target_args(Some(namespace));
    args.push("exec".into());
    args.push("-it".into());
    args.push(pod.to_owned());
    if let Some(c) = container.filter(|c| !c.is_empty()) {
        args.push("-c".into());
        args.push(c.to_owned());
    }
    args.extend([
        "--".into(),
        "/bin/sh".into(),
        "-c".into(),
        "export TERM=xterm-256color; if [ -x /bin/bash ]; then exec /bin/bash; elif [ -x /bin/ash ]; then exec /bin/ash; else exec /bin/sh; fi".into(),
    ]);
    let title = match container.filter(|c| !c.is_empty()) {
        Some(c) => format!("Pod: {pod} ({c})"),
        None => format!("Pod: {pod}"),
    };
    host::open_terminal(&title, "kubectl", &args)
}

/// Probes whether the target Pod container has `/bin/sh` before opening a Terminal tab.
/// If the container is distroless (missing `/bin/sh`), returns a descriptive error instructing
/// the user to choose one of the `Debug Shell` options instead of opening a failing Terminal tab.
pub fn open_exec_or_debug_terminal(
    project: Option<&Project>,
    cfg: &KubeConfigState,
    namespace: &str,
    pod: &str,
    container: Option<&str>,
) -> Result<(), Error> {
    if let Some(proj) = project {
        let mut probe = cfg.target_args(Some(namespace));
        probe.extend(["exec".into(), pod.to_owned()]);
        if let Some(c) = container.filter(|c| !c.is_empty()) {
            probe.extend(["-c".into(), c.to_owned()]);
        }
        probe.extend(["--".into(), "/bin/sh".into(), "-c".into(), "true".into()]);
        let probe_err = match run_kubectl(proj, &probe, None) {
            Ok(out) if out.status != Some(0) => Some(format!("{} {}", out.stderr, out.stdout)),
            Err(err) => Some(err.to_string()),
            Ok(_) => None,
        };
        if let Some(raw_msg) = probe_err {
            let low = raw_msg.to_lowercase();
            let cname = container.filter(|c| !c.is_empty()).unwrap_or(pod);
            let custom_img = if cfg.debugger_image.trim().is_empty() {
                DEFAULT_DEBUGGER_IMAGE
            } else {
                cfg.debugger_image.trim()
            };
            if low.contains("no such file or directory")
                || low.contains("executable file not found")
                || low.contains("oci runtime exec failed")
                || low.contains("stat /bin/sh")
                || low.contains("unable to start container process")
            {
                return Err(Error::Failed(format!(
                    "Container '{cname}' in pod '{pod}' does not have a shell (/bin/sh). Please choose 'Debug Shell (busybox:latest)' or 'Debug Shell ({custom_img})' from the Shell (>_) menu."
                )));
            }
            return Err(Error::Failed(first_line_or(
                &raw_msg,
                "kubectl exec failed",
            )));
        }
    }
    open_exec_terminal(cfg, namespace, pod, container)
}

/// Opens an ephemeral debug container (`kubectl debug --image=<image>`) for Pods,
/// automatically terminating the ephemeral container when the Terminal tab is closed.
pub fn open_debug_terminal(
    project: Option<&Project>,
    cfg: &KubeConfigState,
    namespace: &str,
    pod: &str,
    container: Option<&str>,
    image: Option<&str>,
) -> Result<(), Error> {
    let custom_default = if cfg.debugger_image.trim().is_empty() {
        DEFAULT_DEBUGGER_IMAGE
    } else {
        cfg.debugger_image.trim()
    };
    let debug_image = match image.map(str::trim).filter(|s| !s.is_empty()) {
        Some("custom") => custom_default,
        Some(explicit) => explicit,
        None => "busybox:latest",
    };
    let title = match container.filter(|c| !c.is_empty()) {
        Some(c) => format!("Debug: {pod} ({c})"),
        None => format!("Debug: {pod}"),
    };

    if let Some(proj) = project {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() % 1_000_000)
            .unwrap_or(1000);
        let dbg_name = format!("debugger-{secs}");

        let mut create_args = cfg.target_args(Some(namespace));
        create_args.extend([
            "debug".into(),
            pod.to_owned(),
            "--attach=false".into(),
            format!("--container={dbg_name}"),
            format!("--image={debug_image}"),
        ]);
        if let Some(c) = container.filter(|c| !c.is_empty()) {
            create_args.push(format!("--target={c}"));
        }
        create_args.extend([
            "--".into(),
            "/bin/sh".into(),
            "-c".into(),
            "rm -f /tmp/.openidea-dbg-stop; trap 'exit 0' TERM INT HUP USR1; i=0; while [ ! -f /tmp/.openidea-dbg-stop ] && [ $i -lt 7200 ]; do sleep 1; i=$((i+1)); done; exit 0".into(),
        ]);
        let out = run_kubectl(proj, &create_args, None)?;
        if out.status != Some(0) {
            return Err(Error::Failed(first_line_or(
                &out.stderr,
                "kubectl debug failed",
            )));
        }

        // Wait until the ephemeral container is running and read its containerID + nodeName
        let mut node_name = String::new();
        let mut cid = String::new();
        for _ in 0..30 {
            let mut get_args = cfg.target_args(Some(namespace));
            get_args.extend([
                "get".into(),
                "pod".into(),
                pod.to_owned(),
                "-o".into(),
                "json".into(),
            ]);
            if let Ok(gout) = run_kubectl(proj, &get_args, None)
                && gout.status == Some(0)
                && let Ok(v) = serde_json::from_str::<Value>(&gout.stdout)
            {
                if node_name.is_empty() {
                    node_name = v
                        .pointer("/spec/nodeName")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                }
                if let Some(arr) = v
                    .pointer("/status/ephemeralContainerStatuses")
                    .and_then(Value::as_array)
                {
                    for ecs in arr {
                        if ecs.get("name").and_then(Value::as_str) == Some(dbg_name.as_str())
                            && ecs.pointer("/state/running").is_some()
                        {
                            cid = ecs
                                .get("containerID")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .split("://")
                                .last()
                                .unwrap_or("")
                                .to_owned();
                            break;
                        }
                    }
                }
                if !cid.is_empty() {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }

        // Launch a self-deleting node watcher pod that monitors containerd's open -stdout FIFOs for `cid`
        // and touches `/tmp/.openidea-dbg-stop` inside `dbg_name` as soon as `kubectl exec -it` disconnects.
        if !node_name.is_empty() && !cid.is_empty() {
            let watch_pod = format!("node-dbg-watch-{secs}");
            let watcher_entrypoint = format!(
                concat!(
                    "CID='{cid}'; ",
                    "CPID=$(pidof containerd 2>/dev/null | awk '{{print $1}}'); ",
                    "if [ -n \"$CID\" ] && [ -n \"$CPID\" ]; then ",
                    "  BASE=$(ls -l /proc/$CPID/fd 2>/dev/null | grep -F \"$CID\" | grep -c -- '-stdout$'); ",
                    "  i=0; ATTACHED=0; ",
                    "  while true; do ",
                    "    CUR=$(ls -l /proc/$CPID/fd 2>/dev/null | grep -F \"$CID\" | grep -c -- '-stdout$'); ",
                    "    if [ \"$CUR\" -gt \"$BASE\" ]; then ATTACHED=1; ",
                    "    elif [ \"$ATTACHED\" -eq 1 ] || [ \"$CUR\" -eq 0 ]; then ",
                    "      for kc in /etc/kubernetes/admin.conf /etc/kubernetes/kubelet.conf /var/lib/kubelet/kubeconfig; do ",
                    "        if [ -f \"/host$kc\" ]; then ",
                    "          chroot /host kubectl --kubeconfig=\"$kc\" exec {pod} -n {ns} -c {dbg} -- touch /tmp/.openidea-dbg-stop >/dev/null 2>&1 || true; ",
                    "          chroot /host kubectl --kubeconfig=\"$kc\" delete pod {watch_pod} -n default --force --grace-period=0 >/dev/null 2>&1 && break; ",
                    "        fi; ",
                    "      done; ",
                    "      exit 0; ",
                    "    else i=$((i+1)); if [ $i -ge 30 ]; then ",
                    "      for kc in /etc/kubernetes/admin.conf /etc/kubernetes/kubelet.conf /var/lib/kubelet/kubeconfig; do ",
                    "        if [ -f \"/host$kc\" ]; then ",
                    "          chroot /host kubectl --kubeconfig=\"$kc\" exec {pod} -n {ns} -c {dbg} -- touch /tmp/.openidea-dbg-stop >/dev/null 2>&1 || true; ",
                    "          chroot /host kubectl --kubeconfig=\"$kc\" delete pod {watch_pod} -n default --force --grace-period=0 >/dev/null 2>&1 && break; ",
                    "        fi; ",
                    "      done; ",
                    "      exit 0; ",
                    "    fi; fi; ",
                    "    sleep 1; ",
                    "  done; ",
                    "fi"
                ),
                cid = cid,
                pod = pod,
                ns = namespace,
                dbg = dbg_name,
                watch_pod = watch_pod
            );
            let manifest = json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": {
                    "name": watch_pod,
                    "namespace": "default",
                    "labels": {
                        "app.kubernetes.io/managed-by": "openidea-dbg-watch"
                    }
                },
                "spec": {
                    "nodeName": node_name,
                    "hostPID": true,
                    "hostNetwork": true,
                    "restartPolicy": "Never",
                    "terminationGracePeriodSeconds": 0,
                    "tolerations": [{ "operator": "Exists" }],
                    "containers": [{
                        "name": "watcher",
                        "image": "busybox:latest",
                        "securityContext": { "privileged": true },
                        "command": ["/bin/sh", "-c", watcher_entrypoint],
                        "volumeMounts": [{ "name": "host-root", "mountPath": "/host" }]
                    }],
                    "volumes": [{ "name": "host-root", "hostPath": { "path": "/" } }]
                }
            });
            let mut apply_args = cfg.target_args(Some("default"));
            apply_args.extend(["apply".into(), "-f".into(), "-".into()]);
            let _ = run_kubectl(proj, &apply_args, Some(&manifest.to_string()));
        }

        let mut exec_args = cfg.target_args(Some(namespace));
        exec_args.extend([
            "exec".into(),
            "-it".into(),
            pod.to_owned(),
            "-c".into(),
            dbg_name,
            "--".into(),
            "/bin/sh".into(),
            "-c".into(),
            "trap 'touch /tmp/.openidea-dbg-stop' EXIT HUP INT TERM; export TERM=xterm-256color; if [ -x /bin/bash ]; then /bin/bash; elif [ -x /bin/ash ]; then /bin/ash; else /bin/sh; fi; touch /tmp/.openidea-dbg-stop".into(),
        ]);
        return host::open_terminal(&title, "kubectl", &exec_args);
    }

    let mut args = cfg.target_args(Some(namespace));
    args.extend([
        "debug".into(),
        "-it".into(),
        pod.to_owned(),
        format!("--image={debug_image}"),
    ]);
    if let Some(c) = container.filter(|c| !c.is_empty()) {
        args.push(format!("--target={c}"));
    }
    args.extend([
        "--".into(),
        "/bin/sh".into(),
        "-c".into(),
        "export TERM=xterm-256color; if [ -x /bin/bash ]; then exec /bin/bash; elif [ -x /bin/ash ]; then exec /bin/ash; else exec /bin/sh; fi".into(),
    ]);
    host::open_terminal(&title, "kubectl", &args)
}

/// Opens a shell on a Kubernetes Node (`kubectl exec -it node-debugger-<node>`)
/// supporting 3 modes:
/// - `"host"` (default): enters the Node's own OS and `/bin/bash` via `nsenter -t 1 -m -u -i -n -p` (with `chroot /host` fallback)
/// - `"busybox"`: uses `busybox:latest` shell with the Node's root filesystem mounted at `/host`
/// - `"custom"` (or any custom `image:tag`): uses the configured `debuggerImage` (`cfg.debugger_image`) with the Node's root filesystem mounted at `/host`
pub fn open_node_shell_terminal(
    project: Option<&Project>,
    cfg: &KubeConfigState,
    node: &str,
    mode: Option<&str>,
) -> Result<(), Error> {
    let custom_default = if cfg.debugger_image.trim().is_empty() {
        DEFAULT_DEBUGGER_IMAGE
    } else {
        cfg.debugger_image.trim()
    };
    let shell_mode = mode
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("host");
    let image = match shell_mode {
        "host" | "busybox" | "busybox:latest" => "busybox:latest",
        "custom" => custom_default,
        custom => custom,
    };
    let sanitized: String = node
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_owned();
    let suffix = if sanitized.len() > 42 {
        &sanitized[..42]
    } else {
        &sanitized
    };
    let pod_name = format!("node-debugger-{suffix}");

    if let Some(proj) = project {
        let mut del_args = cfg.target_args(Some("default"));
        del_args.extend([
            "delete".into(),
            "pod".into(),
            pod_name.clone(),
            "--force".into(),
            "--grace-period=0".into(),
            "--ignore-not-found=true".into(),
        ]);
        let _ = run_kubectl(proj, &del_args, None);

        let entrypoint = format!(
            concat!(
                "CID=$(ls -l /proc/$PPID/fd 2>/dev/null | grep -- '-stdout$' | head -n1 | sed 's|.*/containers/||;s|/io/.*||'); ",
                "CPID=$(pidof containerd 2>/dev/null | awk '{{print $1}}'); ",
                "if [ -n \"$CID\" ] && [ -n \"$CPID\" ]; then ",
                "  BASE=$(ls -l /proc/$CPID/fd 2>/dev/null | grep -F \"$CID\" | grep -c -- '-stdout$'); ",
                "  i=0; ATTACHED=0; ",
                "  while true; do ",
                "    CUR=$(ls -l /proc/$CPID/fd 2>/dev/null | grep -F \"$CID\" | grep -c -- '-stdout$'); ",
                "    if [ \"$CUR\" -gt \"$BASE\" ]; then ATTACHED=1; ",
                "    elif [ \"$ATTACHED\" -eq 1 ]; then ",
                "      for kc in /etc/kubernetes/admin.conf /etc/kubernetes/kubelet.conf /var/lib/kubelet/kubeconfig; do ",
                "        if [ -f \"/host$kc\" ]; then ",
                "          chroot /host kubectl --kubeconfig=\"$kc\" delete pod {} -n default --force --grace-period=0 >/dev/null 2>&1 && break; ",
                "        fi; ",
                "      done; ",
                "      exit 0; ",
                "    else i=$((i+1)); if [ $i -ge 30 ]; then exit 0; fi; fi; ",
                "    sleep 1; ",
                "  done; ",
                "else ",
                "  trap 'exit 0' TERM INT HUP USR1; sleep 3600; ",
                "fi"
            ),
            pod_name
        );

        let manifest = json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": pod_name,
                "namespace": "default",
                "labels": {
                    "app.kubernetes.io/managed-by": "openidea-node-shell",
                    "openidea.io/node": suffix,
                }
            },
            "spec": {
                "nodeName": node,
                "hostPID": true,
                "hostNetwork": true,
                "hostIPC": true,
                "restartPolicy": "Never",
                "terminationGracePeriodSeconds": 0,
                "tolerations": [
                    { "operator": "Exists" }
                ],
                "containers": [
                    {
                        "name": "debugger",
                        "image": image,
                        "securityContext": {
                            "privileged": true
                        },
                        "command": ["/bin/sh", "-c", entrypoint],
                        "volumeMounts": [
                            {
                                "name": "host-root",
                                "mountPath": "/host"
                            }
                        ]
                    }
                ],
                "volumes": [
                    {
                        "name": "host-root",
                        "hostPath": {
                            "path": "/"
                        }
                    }
                ]
            }
        });

        let mut apply_args = cfg.target_args(Some("default"));
        apply_args.extend(["apply".into(), "-f".into(), "-".into()]);
        let out = run_kubectl(proj, &apply_args, Some(&manifest.to_string()))?;
        if out.status != Some(0) {
            return Err(Error::Failed(first_line_or(
                &out.stderr,
                "failed to create node-debugger pod",
            )));
        }

        let mut wait_args = cfg.target_args(Some("default"));
        wait_args.extend([
            "wait".into(),
            "--for=condition=Ready".into(),
            format!("pod/{pod_name}"),
            "--timeout=30s".into(),
        ]);
        let _ = run_kubectl(proj, &wait_args, None);
    }

    let exec_cmd = if shell_mode == "host" {
        "trap 'kill -USR1 1 2>/dev/null' EXIT HUP INT TERM; export TERM=xterm-256color; if command -v nsenter >/dev/null 2>&1 && [ -d /proc/1/ns ]; then nsenter -t 1 -m -u -i -n -p -- /bin/sh -c 'if [ -x /bin/bash ]; then exec /bin/bash; else exec /bin/sh; fi'; elif [ -d /host ]; then chroot /host /bin/sh -c 'if [ -x /bin/bash ]; then exec /bin/bash; else exec /bin/sh; fi'; else /bin/sh; fi"
    } else {
        "trap 'kill -USR1 1 2>/dev/null' EXIT HUP INT TERM; export TERM=xterm-256color; if [ -d /host ]; then cd /host; fi; if [ -x /bin/bash ]; then /bin/bash; elif [ -x /bin/ash ]; then /bin/ash; else /bin/sh; fi"
    };

    let title = match shell_mode {
        "host" => format!("Node: {node}"),
        "busybox" | "busybox:latest" => format!("Node (busybox): {node}"),
        _ => format!("Node (k8sdebugger): {node}"),
    };

    let mut args = cfg.target_args(Some("default"));
    args.extend([
        "exec".into(),
        "-it".into(),
        pod_name,
        "--".into(),
        "/bin/sh".into(),
        "-c".into(),
        exec_cmd.into(),
    ]);
    host::open_terminal(&title, "kubectl", &args)
}

/// Runs a shell command inside a Pod container (`kubectl exec`) and returns structured output + updated working directory.
pub fn exec_pod_command(
    project: &Project,
    cfg: &KubeConfigState,
    namespace: &str,
    pod: &str,
    container: Option<&str>,
    command: &str,
    cwd: &str,
) -> Value {
    let start_dir = if cwd.trim().is_empty() {
        "/"
    } else {
        cwd.trim()
    };
    let wrapped = format!(
        "cd '{}' 2>/dev/null; {}; printf '\\n__OPENIDEA_PWD__:%s' \"$(pwd 2>/dev/null || echo '{}')\"",
        start_dir.replace('\'', "'\\''"),
        command,
        start_dir
    );
    let mut args = cfg.target_args(Some(namespace));
    args.push("exec".into());
    args.push(pod.to_owned());
    if let Some(c) = container.filter(|c| !c.is_empty()) {
        args.push("-c".into());
        args.push(c.to_owned());
    }
    args.extend(["--".into(), "/bin/sh".into(), "-c".into(), wrapped]);

    match run_kubectl(project, &args, None) {
        Ok(out) => {
            let no_shell = out.stderr.contains("stat /bin/sh")
                || out.stderr.contains("executable file not found")
                || out.stderr.contains("no such file or directory") && out.stdout.is_empty();
            if no_shell {
                // Distroless container fallback: try running the command directly without `/bin/sh -c`
                let direct_tokens: Vec<String> =
                    command.split_whitespace().map(str::to_owned).collect();
                if !direct_tokens.is_empty() && !command.contains("&&") && !command.contains(';') {
                    let mut d_args = cfg.target_args(Some(namespace));
                    d_args.push("exec".into());
                    d_args.push(pod.to_owned());
                    if let Some(c) = container.filter(|c| !c.is_empty()) {
                        d_args.push("-c".into());
                        d_args.push(c.to_owned());
                    }
                    d_args.push("--".into());
                    d_args.extend(direct_tokens);
                    if let Ok(d_out) = run_kubectl(project, &d_args, None)
                        && (d_out.status == Some(0) || !d_out.stdout.is_empty())
                    {
                        return json!({
                            "op": "shell-output",
                            "command": command,
                            "stdout": d_out.stdout,
                            "stderr": d_out.stderr,
                            "exitCode": d_out.status.unwrap_or(0),
                            "cwd": start_dir,
                            "distroless": true,
                        });
                    }
                }
                return json!({
                    "op": "shell-output",
                    "command": command,
                    "stdout": "",
                    "stderr": format!(
                        "{}\n(Note: Container image appears to be distroless without /bin/sh. Use 'Debug Container (busybox)' above to attach an ephemeral shell.)",
                        out.stderr.trim()
                    ),
                    "exitCode": out.status.unwrap_or(126),
                    "cwd": start_dir,
                    "distroless": true,
                });
            }

            let mut stdout = out.stdout;
            let mut next_cwd = start_dir.to_owned();
            if let Some(idx) = stdout.rfind("\n__OPENIDEA_PWD__:") {
                let pwd_part = stdout[idx + "\n__OPENIDEA_PWD__:".len()..].trim();
                if !pwd_part.is_empty() {
                    next_cwd = pwd_part.to_owned();
                }
                stdout.truncate(idx);
            } else if let Some(idx) = stdout.find("__OPENIDEA_PWD__:") {
                let pwd_part = stdout[idx + "__OPENIDEA_PWD__:".len()..].trim();
                if !pwd_part.is_empty() {
                    next_cwd = pwd_part.to_owned();
                }
                stdout.truncate(idx);
            }
            json!({
                "op": "shell-output",
                "command": command,
                "stdout": stdout,
                "stderr": out.stderr,
                "exitCode": out.status.unwrap_or(0),
                "cwd": next_cwd,
                "distroless": false,
            })
        }
        Err(e) => json!({
            "op": "shell-output",
            "command": command,
            "stdout": "",
            "stderr": e.to_string(),
            "exitCode": 1,
            "cwd": start_dir,
            "distroless": false,
        }),
    }
}

/// Opens a terminal running `kubectl port-forward`.
/// Never defaults to forwarding onto the same local port number: when `local_port` is omitted/empty,
/// binds a random available local port (`:<target_port>` or an ephemeral port `49152..=65000` when `open_in_browser` is true).
#[allow(clippy::too_many_arguments)]
pub fn open_port_forward_terminal(
    cfg: &KubeConfigState,
    kind: &str,
    namespace: &str,
    name: &str,
    port_spec: &str,
    local_port: Option<&str>,
    address: Option<&str>,
    https: bool,
    open_in_browser: bool,
) -> Result<(), Error> {
    let mut args = cfg.target_args(Some(namespace));
    args.push("port-forward".into());
    let addr = address
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("localhost");
    if addr != "localhost" && addr != "127.0.0.1" {
        args.push(format!("--address={addr}"));
    }
    let resource_target = format!("{}/{name}", kind.to_ascii_lowercase());
    args.push(resource_target);

    let target_port = if port_spec.trim().is_empty() {
        "8080"
    } else {
        port_spec.trim()
    };
    let clean_local = local_port
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("random") && *s != "0");

    let (mapping, resolved_local_port): (String, Option<String>) = if target_port.contains(':') {
        let lp = target_port
            .split(':')
            .next()
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        (target_port.to_owned(), lp)
    } else if let Some(lp) = clean_local {
        (format!("{lp}:{target_port}"), Some(lp.to_owned()))
    } else if open_in_browser {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(12345);
        let ephem = 49152 + (nanos % 15000);
        (format!("{ephem}:{target_port}"), Some(ephem.to_string()))
    } else {
        (format!(":{target_port}"), None)
    };

    args.push(mapping.clone());
    host::open_terminal(
        &format!("Port Forward: {name} ({mapping})"),
        "kubectl",
        &args,
    )?;

    if open_in_browser && let Some(lp) = resolved_local_port {
        let scheme = if https { "https" } else { "http" };
        let first_host = addr
            .split(',')
            .next()
            .map(str::trim)
            .filter(|h| !h.is_empty() && *h != "0.0.0.0" && *h != "::")
            .unwrap_or("localhost");
        let _ = host::open_url(&format!("{scheme}://{first_host}:{lp}"));
    }
    Ok(())
}

/// Scales a workload (`kubectl scale --replicas=N`).
pub fn scale_workload(
    project: &Project,
    cfg: &KubeConfigState,
    kind: &str,
    namespace: &str,
    name: &str,
    replicas: u32,
) -> Result<String, Error> {
    let mut args = cfg.target_args(Some(namespace));
    args.extend([
        "scale".into(),
        format!("{}/{name}", kind.to_ascii_lowercase()),
        format!("--replicas={replicas}"),
    ]);
    let out = run_kubectl(project, &args, None)?;
    if out.status != Some(0) {
        return Err(Error::Failed(first_line_or(
            &out.stderr,
            "kubectl scale failed",
        )));
    }
    Ok(out.stdout.trim().to_owned())
}

/// Restarts a workload rollout (`kubectl rollout restart`).
pub fn rollout_restart(
    project: &Project,
    cfg: &KubeConfigState,
    kind: &str,
    namespace: &str,
    name: &str,
) -> Result<String, Error> {
    let mut args = cfg.target_args(Some(namespace));
    args.extend([
        "rollout".into(),
        "restart".into(),
        format!("{}/{name}", kind.to_ascii_lowercase()),
    ]);
    let out = run_kubectl(project, &args, None)?;
    if out.status != Some(0) {
        return Err(Error::Failed(first_line_or(
            &out.stderr,
            "kubectl rollout restart failed",
        )));
    }
    Ok(out.stdout.trim().to_owned())
}

/// Deletes a cluster resource (`kubectl delete <kind> <name>`).
pub fn delete_cluster_resource(
    project: &Project,
    cfg: &KubeConfigState,
    kind: &str,
    namespace: &str,
    name: &str,
) -> Result<String, Error> {
    delete_cluster_resource_opts(project, cfg, kind, namespace, name, false)
}

/// Deletes a cluster resource, optionally forcing immediate removal (`--force --grace-period=0`).
pub fn delete_cluster_resource_opts(
    project: &Project,
    cfg: &KubeConfigState,
    kind: &str,
    namespace: &str,
    name: &str,
    force: bool,
) -> Result<String, Error> {
    if kind == "HelmRelease" {
        let mut args = Vec::new();
        if !cfg.kubeconfig.trim().is_empty() {
            args.push(format!("--kubeconfig={}", cfg.kubeconfig.trim()));
        }
        if !cfg.context.trim().is_empty() {
            args.push(format!("--kube-context={}", cfg.context.trim()));
        }
        if !namespace.trim().is_empty() {
            args.push("-n".into());
            args.push(namespace.trim().to_owned());
        }
        args.extend(["uninstall".into(), name.to_owned()]);
        let out = project.run_process("helm", &args, None)?;
        if out.status != Some(0) {
            return Err(Error::Failed(first_line_or(
                &out.stderr,
                "helm uninstall failed",
            )));
        }
        return Ok(out.stdout.trim().to_owned());
    }
    let target_type = kind
        .strip_prefix("CR:")
        .unwrap_or(kind)
        .to_ascii_lowercase();
    let (_, cluster_scoped, _) = if kind.starts_with("CR:") {
        ("cr", namespace.is_empty(), false)
    } else {
        kubectl_resource_info(kind)
    };
    let mut args = if cluster_scoped || namespace.is_empty() {
        cfg.base_args(false)
    } else {
        cfg.target_args(Some(namespace))
    };
    args.extend([
        "delete".into(),
        target_type,
        name.to_owned(),
        "--wait=false".into(),
    ]);
    if force {
        args.extend(["--force".into(), "--grace-period=0".into()]);
    }
    let out = run_kubectl(project, &args, None)?;
    if out.status != Some(0) {
        return Err(Error::Failed(first_line_or(
            &out.stderr,
            "kubectl delete failed",
        )));
    }
    Ok(out.stdout.trim().to_owned())
}

/// Triggers a manual Job from a CronJob (`kubectl create job --from=cronjob/<name> <name>-manual`).
pub fn trigger_cronjob(
    project: &Project,
    cfg: &KubeConfigState,
    namespace: &str,
    name: &str,
) -> Result<String, Error> {
    let mut args = cfg.target_args(Some(namespace));
    let job_name = format!("{name}-manual");
    let _ = run_kubectl(
        project,
        &[
            cfg.target_args(Some(namespace)),
            vec!["delete".into(), "job".into(), job_name.clone()],
        ]
        .concat(),
        None,
    );
    args.extend([
        "create".into(),
        "job".into(),
        format!("--from=cronjob/{name}"),
        job_name,
    ]);
    let out = run_kubectl(project, &args, None)?;
    if out.status != Some(0) {
        return Err(Error::Failed(first_line_or(
            &out.stderr,
            "kubectl create job --from=cronjob failed",
        )));
    }
    Ok(first_line_or(
        &out.stdout,
        &format!("Triggered job from cronjob/{name}"),
    ))
}

/// Suspends or resumes a CronJob (`kubectl patch cronjob <name> -p '{"spec":{"suspend":...}}'`).
pub fn suspend_cronjob(
    project: &Project,
    cfg: &KubeConfigState,
    namespace: &str,
    name: &str,
    suspend: bool,
) -> Result<String, Error> {
    let mut args = cfg.target_args(Some(namespace));
    let patch = format!("{{\"spec\":{{\"suspend\":{suspend}}}}}");
    args.extend([
        "patch".into(),
        "cronjob".into(),
        name.to_owned(),
        "-p".into(),
        patch,
    ]);
    let out = run_kubectl(project, &args, None)?;
    if out.status != Some(0) {
        return Err(Error::Failed(first_line_or(
            &out.stderr,
            "kubectl patch cronjob failed",
        )));
    }
    Ok(first_line_or(
        &out.stdout,
        &format!(
            "cronjob/{name} {}",
            if suspend { "suspended" } else { "resumed" }
        ),
    ))
}

/// Cordons or uncordons a cluster Node (`kubectl cordon <node>` / `kubectl uncordon <node>`).
pub fn cordon_node(
    project: &Project,
    cfg: &KubeConfigState,
    name: &str,
    uncordon: bool,
) -> Result<String, Error> {
    let mut args = cfg.base_args(false);
    let op = if uncordon { "uncordon" } else { "cordon" };
    args.extend([op.into(), name.to_owned()]);
    let out = run_kubectl(project, &args, None)?;
    if out.status != Some(0) {
        return Err(Error::Failed(first_line_or(
            &out.stderr,
            &format!("kubectl {op} failed"),
        )));
    }
    Ok(first_line_or(&out.stdout, &format!("node/{name} {op}ed")))
}

/// Drains a cluster Node (`kubectl drain <node> --ignore-daemonsets --delete-emptydir-data --force`).
pub fn drain_node(project: &Project, cfg: &KubeConfigState, name: &str) -> Result<String, Error> {
    let mut args = cfg.base_args(false);
    args.extend([
        "drain".into(),
        name.to_owned(),
        "--ignore-daemonsets".into(),
        "--delete-emptydir-data".into(),
        "--force".into(),
        "--timeout=45s".into(),
    ]);
    let out = run_kubectl(project, &args, None)?;
    if out.status != Some(0) {
        return Err(Error::Failed(first_line_or(
            &out.stderr,
            "kubectl drain failed",
        )));
    }
    Ok(first_line_or(&out.stdout, &format!("node/{name} drained")))
}

/// Applies, dry-runs, diffs, or deletes a YAML manifest string via `kubectl -f -`.
pub fn apply_or_diff_manifest(
    project: &Project,
    cfg: &KubeConfigState,
    yaml_text: &str,
    subcommand: &str, // "apply", "diff", "delete", "dry-run"
) -> Result<String, Error> {
    let mut args = cfg.target_args(None);
    match subcommand {
        "apply" => {
            args.extend(["apply".into(), "-f".into(), "-".into()]);
        }
        "dry-run" => {
            args.extend([
                "apply".into(),
                "--dry-run=client".into(),
                "-f".into(),
                "-".into(),
            ]);
        }
        "diff" => {
            args.extend(["diff".into(), "-f".into(), "-".into()]);
        }
        "delete" => {
            args.extend(["delete".into(), "-f".into(), "-".into()]);
        }
        other => return Err(Error::Invalid(format!("unknown manifest action {other}"))),
    }
    let out = run_kubectl(project, &args, Some(yaml_text))?;
    // `kubectl diff` exits with status 1 when differences exist, and >1 on error.
    if subcommand == "diff" {
        return match out.status {
            Some(0) => Ok("No differences between manifest and live cluster resource.".into()),
            Some(1) => Ok(out.stdout),
            _ => Err(Error::Failed(first_line_or(
                &out.stderr,
                "kubectl diff failed",
            ))),
        };
    }
    if out.status != Some(0) {
        return Err(Error::Failed(first_line_or(
            &out.stderr,
            &format!("kubectl {subcommand} failed"),
        )));
    }
    Ok(out.stdout.trim().to_owned())
}

/// Builds a Kustomize directory using `kubectl kustomize <dir>` (or `kustomize build <dir>`).
pub fn build_kustomize(project: &Project, dir: &str) -> Result<String, Error> {
    let target = if dir.is_empty() { "." } else { dir };
    let args = vec!["kustomize".to_owned(), target.to_owned()];
    if let Ok(out) = run_kubectl(project, &args, None)
        && out.status == Some(0)
    {
        return Ok(out.stdout);
    }
    let k_args = vec!["build".to_owned(), target.to_owned()];
    let out = project.run_process("kustomize", &k_args, None)?;
    if out.status != Some(0) {
        return Err(Error::Failed(first_line_or(
            &out.stderr,
            "kustomize build failed",
        )));
    }
    Ok(out.stdout)
}

/// Renders Helm chart templates (`helm template <dir>`) or fetches release values/manifest.
pub fn render_helm_template(project: &Project, chart_dir: &str) -> Result<String, Error> {
    let target = if chart_dir.is_empty() { "." } else { chart_dir };
    let args = vec!["template".to_owned(), target.to_owned()];
    let out = project.run_process("helm", &args, None)?;
    if out.status != Some(0) {
        return Err(Error::Failed(first_line_or(
            &out.stderr,
            "helm template failed",
        )));
    }
    Ok(out.stdout)
}

/// Fetches a Helm release's manifest or values (`helm get manifest|values <release>`).
pub fn fetch_helm_release_detail(
    project: &Project,
    cfg: &KubeConfigState,
    namespace: &str,
    release: &str,
    sub: &str, // "manifest" or "values"
) -> Result<String, Error> {
    let mut args = Vec::new();
    if !cfg.kubeconfig.trim().is_empty() {
        args.push(format!("--kubeconfig={}", cfg.kubeconfig.trim()));
    }
    if !cfg.context.trim().is_empty() {
        args.push(format!("--kube-context={}", cfg.context.trim()));
    }
    if !namespace.trim().is_empty() {
        args.push("-n".into());
        args.push(namespace.trim().to_owned());
    }
    args.extend(["get".into(), sub.to_owned(), release.to_owned()]);
    let out = project.run_process("helm", &args, None)?;
    if out.status != Some(0) {
        return Err(Error::Failed(first_line_or(&out.stderr, "helm get failed")));
    }
    Ok(out.stdout)
}

/// Summarizes workspace Kubernetes manifests for the tool window's "Workspace Manifests" section.
pub fn workspace_manifests_json(
    files: &BTreeMap<String, crate::manifest::ParsedFile>,
    root: &str,
) -> Value {
    let norm_root = root.replace('\\', "/");
    let mut items = Vec::new();
    for (path, parsed) in files {
        let rel = path
            .strip_prefix(&norm_root)
            .map(|r| r.trim_start_matches('/'))
            .unwrap_or(path);
        for doc in &parsed.docs {
            if let Some(ref kind) = doc.kind {
                let name = doc.name.as_ref().map_or("(unnamed)", |n| n.value.as_str());
                let ns = doc
                    .namespace
                    .as_ref()
                    .map_or("default", |n| n.value.as_str());
                let api = doc.api_version.as_ref().map_or("", |a| a.value.as_str());
                items.push(json!({
                    "path": rel,
                    "fullPath": path,
                    "line": doc.name.as_ref().map_or(kind.line, |n| n.line),
                    "kind": kind.value,
                    "name": name,
                    "namespace": ns,
                    "apiVersion": api,
                }));
            }
        }
    }
    Value::Array(items)
}

fn first_line_or(stderr: &str, fallback: &str) -> String {
    stderr
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_owned())
        .unwrap_or_else(|| fallback.to_owned())
}

// ============================================================================
// Hybrid Kubernetes Watch Stream (`kubectl get --raw ...?watch=true`)
// ============================================================================

/// Builds the raw URI for a Kubernetes `watch=true` stream starting at `resource_version`.
pub fn build_watch_raw_uri(
    api_prefix: &str,
    plural: &str,
    cluster_scoped: bool,
    cfg: &KubeConfigState,
    resource_version: &str,
    field_selector_name: Option<&str>,
) -> String {
    let base = if cluster_scoped || cfg.all_namespaces || cfg.namespace.trim().is_empty() {
        format!("{api_prefix}/{plural}")
    } else {
        let ns = url_encode_component(cfg.namespace.trim());
        format!("{api_prefix}/namespaces/{ns}/{plural}")
    };
    let mut query = vec![
        "watch=true".to_owned(),
        "allowWatchBookmarks=true".to_owned(),
        "timeoutSeconds=295".to_owned(),
    ];
    if !resource_version.trim().is_empty() {
        query.push(format!(
            "resourceVersion={}",
            url_encode_component(resource_version.trim())
        ));
    }
    if let Some(name) = field_selector_name.filter(|s| !s.trim().is_empty()) {
        let sel = format!("metadata.name={}", name.trim());
        query.push(format!("fieldSelector={}", url_encode_component(&sel)));
    }
    format!("{base}?{}", query.join("&"))
}

/// Resolves the REST `(api_prefix, plural, is_cluster_scoped)` tuple for built-in kinds or `CR:<crd>`.
pub fn resolve_watch_endpoint(
    project: &Project,
    cfg: &KubeConfigState,
    snap: &mut ClusterSnapshot,
    kind: &str,
) -> Option<(String, String, bool)> {
    if let Some((api_prefix, plural, cluster_scoped)) = k8s_api_endpoint_info(kind) {
        return Some((api_prefix.to_owned(), plural.to_owned(), cluster_scoped));
    }
    if let Some(crd_name) = kind.strip_prefix("CR:") {
        if !snap
            .resources
            .iter()
            .any(|r| r.kind == "CustomResourceDefinition")
        {
            fetch_crds_into_snapshot(project, cfg, snap);
        }
        return resolve_crd_api_endpoint(project, cfg, snap, crd_name, Some(4));
    }
    None
}

/// Parsed single line from a Kubernetes newline-delimited JSON `WatchEvent` stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParsedWatchFrame {
    ResourceUpsert {
        resource: Box<ClusterResource>,
        raw_object_json: String,
        resource_version: String,
    },
    ResourceDeleted {
        kind: String,
        namespace: String,
        name: String,
        resource_version: String,
    },
    EventUpsert {
        event: ClusterEvent,
        resource_version: String,
    },
    Bookmark {
        resource_version: String,
    },
    Expired {
        message: String,
    },
    Ignored,
    Malformed,
}

#[derive(Deserialize)]
struct RawWatchEnvelope<'a> {
    #[serde(default, rename = "type")]
    event_type: String,
    #[serde(borrow)]
    object: &'a serde_json::value::RawValue,
}

#[derive(Deserialize)]
struct RawWatchBookmarkMeta {
    #[serde(default)]
    metadata: Option<RawMetadata>,
    #[serde(default)]
    code: Option<u16>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

/// Extracts a resource's `metadata.resourceVersion` from a single-object JSON string.
pub fn extract_object_resource_version(raw_json: &str) -> String {
    serde_json::from_str::<RawWatchBookmarkMeta>(raw_json)
        .ok()
        .and_then(|m| m.metadata)
        .and_then(|m| m.resource_version)
        .unwrap_or_default()
}

/// Parses one JSON line emitted by `kubectl get --raw "...?watch=true"`.
pub fn parse_watch_frame_line(line: &str, expected_kind: &str) -> ParsedWatchFrame {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with("started:") {
        return ParsedWatchFrame::Ignored;
    }
    let Ok(env) = serde_json::from_str::<RawWatchEnvelope<'_>>(trimmed) else {
        return ParsedWatchFrame::Malformed;
    };
    let obj_raw = env.object.get();
    match env.event_type.as_str() {
        "BOOKMARK" => {
            let rv = extract_object_resource_version(obj_raw);
            if rv.is_empty() {
                ParsedWatchFrame::Ignored
            } else {
                ParsedWatchFrame::Bookmark {
                    resource_version: rv,
                }
            }
        }
        "ERROR" => {
            if let Ok(st) = serde_json::from_str::<RawWatchBookmarkMeta>(obj_raw) {
                let reason = st.reason.unwrap_or_default();
                let msg = st.message.unwrap_or_else(|| "watch stream error".into());
                if st.code == Some(410)
                    || reason.contains("Expired")
                    || reason.contains("Gone")
                    || msg.contains("too old")
                {
                    return ParsedWatchFrame::Expired { message: msg };
                }
            }
            ParsedWatchFrame::Expired {
                message: "watch stream closed by apiserver".into(),
            }
        }
        "ADDED" | "MODIFIED" | "DELETED" => {
            if expected_kind == "Event" {
                let Ok(ev_item) = serde_json::from_str::<RawEventItem>(obj_raw) else {
                    return ParsedWatchFrame::Malformed;
                };
                let rv = ev_item
                    .metadata
                    .as_ref()
                    .and_then(|m| m.resource_version.clone())
                    .unwrap_or_default();
                if env.event_type == "DELETED" {
                    return ParsedWatchFrame::Bookmark {
                        resource_version: rv,
                    };
                }
                let (obj_kind, obj_name) = match ev_item.involved_object {
                    Some(inv) => (
                        inv.kind.unwrap_or_else(|| "Resource".into()),
                        inv.name.unwrap_or_default(),
                    ),
                    None => ("Resource".into(), String::new()),
                };
                let (namespace, created) = match ev_item.metadata {
                    Some(m) => (
                        m.namespace.unwrap_or_else(|| "default".into()),
                        m.creation_timestamp.unwrap_or_default(),
                    ),
                    None => ("default".into(), String::new()),
                };
                let last_seen = ev_item
                    .last_timestamp
                    .or(ev_item.event_time)
                    .unwrap_or(created);
                return ParsedWatchFrame::EventUpsert {
                    event: ClusterEvent {
                        kind: obj_kind,
                        object: obj_name,
                        namespace,
                        reason: ev_item.reason.unwrap_or_default(),
                        message: ev_item.message.unwrap_or_default(),
                        event_type: ev_item.event_type.unwrap_or_else(|| "Normal".into()),
                        count: ev_item.count.unwrap_or(1),
                        last_seen,
                    },
                    resource_version: rv,
                };
            }

            let Ok(mut raw_item) = serde_json::from_str::<RawResourceItem>(obj_raw) else {
                return ParsedWatchFrame::Malformed;
            };
            let rv = raw_item
                .metadata
                .as_ref()
                .and_then(|m| m.resource_version.clone())
                .unwrap_or_default();
            if expected_kind.starts_with("CR:")
                || raw_item.kind.as_deref().is_none_or(str::is_empty)
            {
                raw_item.kind = Some(expected_kind.to_owned());
            }
            if env.event_type == "DELETED" {
                let Some(meta) = raw_item.metadata else {
                    return ParsedWatchFrame::Ignored;
                };
                let Some(name) = meta.name else {
                    return ParsedWatchFrame::Ignored;
                };
                return ParsedWatchFrame::ResourceDeleted {
                    kind: expected_kind.to_owned(),
                    namespace: meta.namespace.unwrap_or_default(),
                    name,
                    resource_version: rv,
                };
            }
            let Some(res) = parse_raw_resource_item(raw_item) else {
                return ParsedWatchFrame::Ignored;
            };
            ParsedWatchFrame::ResourceUpsert {
                resource: Box::new(res),
                raw_object_json: obj_raw.to_owned(),
                resource_version: rv,
            }
        }
        _ => ParsedWatchFrame::Ignored,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchSpec {
    pub context: String,
    pub kubeconfig: String,
    pub namespace: String,
    pub all_namespaces: bool,
    pub kind: String,
    pub api_prefix: String,
    pub plural: String,
    pub cluster_scoped: bool,
    pub field_selector_name: Option<String>,
}

pub struct ActiveWatchStream {
    pub spec: WatchSpec,
    pub resource_version: String,
    pub child: host::Child,
    pub remainder: String,
}

impl ActiveWatchStream {
    pub fn start(
        project: &Project,
        cfg: &KubeConfigState,
        spec: WatchSpec,
        resource_version: String,
    ) -> Result<Self, Error> {
        let mut eff_cfg = cfg.clone();
        eff_cfg.context = spec.context.clone();
        eff_cfg.kubeconfig = spec.kubeconfig.clone();
        eff_cfg.namespace = spec.namespace.clone();
        eff_cfg.all_namespaces = spec.all_namespaces;

        let uri = build_watch_raw_uri(
            &spec.api_prefix,
            &spec.plural,
            spec.cluster_scoped,
            &eff_cfg,
            &resource_version,
            spec.field_selector_name.as_deref(),
        );
        let mut args = eff_cfg.base_args(false);
        args.extend(["get".into(), "--raw".into(), uri]);
        let cmd = host::Command::new("kubectl").args(args);
        let child = host::start(&cmd, Some(project))?;
        Ok(Self {
            spec,
            resource_version,
            child,
            remainder: String::new(),
        })
    }

    /// Drains available newline-delimited JSON frames without blocking.
    /// Returns `(frames, stream_ended, overflow_or_gone)`.
    pub fn drain_frames(&mut self) -> (Vec<ParsedWatchFrame>, bool, bool) {
        let mut frames = Vec::new();
        let Ok(chunk) = self.child.poll(512 * 1024) else {
            return (frames, true, true);
        };
        let ended = chunk.status.is_some();
        let mut gone = chunk.stderr.contains("410")
            || chunk.stderr.contains("Expired")
            || chunk.stderr.contains("too old");

        if !chunk.stdout.is_empty() {
            // Guard against 1 MiB host ring-buffer overwrite dropping bytes mid-line.
            if chunk.stdout.len() >= 950 * 1024 {
                return (frames, true, true);
            }
            self.remainder.push_str(&chunk.stdout);
            if self.remainder.len() > 2 * 1024 * 1024 {
                self.remainder.clear();
                return (frames, true, true);
            }
            while let Some(nl) = self.remainder.find('\n') {
                let line: String = self.remainder.drain(..=nl).collect();
                let parsed = parse_watch_frame_line(&line, &self.spec.kind);
                match &parsed {
                    ParsedWatchFrame::ResourceUpsert {
                        resource_version, ..
                    }
                    | ParsedWatchFrame::ResourceDeleted {
                        resource_version, ..
                    }
                    | ParsedWatchFrame::EventUpsert {
                        resource_version, ..
                    }
                    | ParsedWatchFrame::Bookmark {
                        resource_version, ..
                    } => {
                        if !resource_version.is_empty() {
                            self.resource_version = resource_version.clone();
                        }
                    }
                    ParsedWatchFrame::Expired { .. } | ParsedWatchFrame::Malformed => {
                        gone = true;
                    }
                    ParsedWatchFrame::Ignored => {}
                }
                if !matches!(
                    parsed,
                    ParsedWatchFrame::Ignored | ParsedWatchFrame::Malformed
                ) {
                    frames.push(parsed);
                }
                if gone {
                    break;
                }
            }
        }
        if ended && chunk.status != Some(0) && frames.is_empty() && !chunk.stderr.trim().is_empty()
        {
            gone = true;
        }
        (frames, ended, gone)
    }

    pub fn stop(self) {
        let _ = self.child.kill();
    }
}

/// Persistent `kubectl proxy --port=0 --address=127.0.0.1` loopback tunnel for the active context,
/// allowing `host::fetch` (`net:127.0.0.1`) to send custom `Accept: application/json;as=Table` +
/// `Accept-Encoding: gzip` + `?resourceVersion=0&includeObject=None` queries to `kube-apiserver`'s
/// watch cache (~2.3s / 1.6 MiB for 88k pods instead of 880 MB across 177 `kubectl get` chunks).
pub struct KubeApiProxy {
    pub kubeconfig: String,
    pub context: String,
    pub port: u16,
    child: host::Child,
}

impl KubeApiProxy {
    pub fn stop(self) {
        let _ = self.child.kill();
    }
}

/// Manages concurrent `kubectl --raw ?watch=true` child processes
/// (up to `MAX_MULTI_NAMESPACE_WATCHES` for the active table category when `total <= 1,000`,
/// 1 for the open Drawer resource, and 1 reusable `kubectl proxy` tunnel), staying safely within
/// OpenIDEA's 8-child-per-plugin cap.
#[derive(Default)]
pub struct WatchManager {
    pub category_streams: Vec<ActiveWatchStream>,
    pub drawer_stream: Option<ActiveWatchStream>,
    pub api_proxy: Option<KubeApiProxy>,
}

impl WatchManager {
    pub fn stop_all(&mut self) {
        self.stop_watches();
        self.stop_proxy();
    }

    pub fn stop_watches(&mut self) {
        self.stop_category();
        self.stop_drawer();
    }

    pub fn stop_category(&mut self) {
        for s in self.category_streams.drain(..) {
            s.stop();
        }
    }

    pub fn stop_drawer(&mut self) {
        if let Some(s) = self.drawer_stream.take() {
            s.stop();
        }
    }

    pub fn stop_proxy(&mut self) {
        if let Some(p) = self.api_proxy.take() {
            p.stop();
        }
    }

    pub fn ensure_api_proxy(&mut self, project: &Project, cfg: &KubeConfigState) -> Option<u16> {
        let want_kc = cfg.kubeconfig.trim();
        let want_ctx = cfg.context.trim();
        if let Some(ref existing) = self.api_proxy
            && existing.kubeconfig == want_kc
            && existing.context == want_ctx
            && let Ok(chunk) = existing.child.wait(0)
            && chunk.status.is_none()
        {
            return Some(existing.port);
        }
        self.stop_proxy();

        let mut args = cfg.base_args(false);
        args.extend([
            "proxy".into(),
            "--port=0".into(),
            "--address=127.0.0.1".into(),
        ]);
        let cmd = host::Command::new("kubectl").args(args);
        let child = host::start(&cmd, Some(project)).ok()?;
        let mut banner = String::new();
        for _ in 0..6 {
            match child.wait(400) {
                Ok(chunk) => {
                    banner.push_str(&chunk.stdout);
                    if let Some(port) = parse_kubectl_proxy_port(&banner) {
                        self.api_proxy = Some(KubeApiProxy {
                            kubeconfig: want_kc.to_owned(),
                            context: want_ctx.to_owned(),
                            port,
                            child,
                        });
                        return Some(port);
                    }
                    if chunk.status.is_some() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = child.kill();
        None
    }

    #[cfg(test)]
    pub fn install_test_proxy(
        &mut self,
        kubeconfig: &str,
        context: &str,
        port: u16,
        child: host::Child,
    ) {
        self.stop_proxy();
        self.api_proxy = Some(KubeApiProxy {
            kubeconfig: kubeconfig.trim().to_owned(),
            context: context.trim().to_owned(),
            port,
            child,
        });
    }
}

fn parse_kubectl_proxy_port(stdout: &str) -> Option<u16> {
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(colon_pos) = trimmed.rfind(':') {
            let tail = trimmed[colon_pos + 1..].trim();
            if let Ok(port) = tail.parse::<u16>()
                && port > 0
            {
                return Some(port);
            }
        }
    }
    None
}

/// Maximum number of matched rows returned to the UI from a ground-truth cluster search.
/// `total_matches` and `scanned_total` always reflect the exact un-truncated counts across the scope.
pub const MAX_SEARCH_RESULTS: usize = 500;

/// Outcome of a ground-truth cluster search across an entire category scope when local pagination
/// holds only a partial window of the cluster's items.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CategorySearchOutcome {
    pub category: String,
    pub query: String,
    pub resources: Vec<ClusterResource>,
    pub events: Vec<ClusterEvent>,
    pub total_matches: u64,
    pub scanned_total: u64,
    pub truncated: bool,
    pub error: Option<String>,
}

#[inline]
pub fn contains_case_insensitive(haystack: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    if needle_lower.is_ascii() {
        let hb = haystack.as_bytes();
        let nb = needle_lower.as_bytes();
        if nb.len() > hb.len() {
            return false;
        }
        let first = nb[0];
        for i in 0..=(hb.len() - nb.len()) {
            if hb[i].to_ascii_lowercase() == first && hb[i..i + nb.len()].eq_ignore_ascii_case(nb) {
                return true;
            }
        }
        false
    } else {
        haystack.to_lowercase().contains(needle_lower)
    }
}

#[derive(Clone, Debug)]
enum SearchToken {
    Any(String),
    Namespace(String),
    Node(String),
    Status(String),
    Ip(String),
}

fn parse_search_tokens(raw_query: &str) -> Vec<SearchToken> {
    raw_query
        .split_whitespace()
        .filter(|tok| !tok.is_empty())
        .map(|tok| {
            let lower = tok.to_lowercase();
            if let Some(v) = lower
                .strip_prefix("ns:")
                .or_else(|| lower.strip_prefix("namespace:"))
                .filter(|s| !s.is_empty())
            {
                SearchToken::Namespace(v.to_owned())
            } else if let Some(v) = lower.strip_prefix("node:").filter(|s| !s.is_empty()) {
                SearchToken::Node(v.to_owned())
            } else if let Some(v) = lower
                .strip_prefix("status:")
                .or_else(|| lower.strip_prefix("phase:"))
                .filter(|s| !s.is_empty())
            {
                SearchToken::Status(v.to_owned())
            } else if let Some(v) = lower.strip_prefix("ip:").filter(|s| !s.is_empty()) {
                SearchToken::Ip(v.to_owned())
            } else {
                SearchToken::Any(lower)
            }
        })
        .collect()
}

fn pod_fields_match_tokens(
    tokens: &[SearchToken],
    namespace: &str,
    name: &str,
    status: &str,
    pod_ip: &str,
    node_name: &str,
    detail_hint: &str,
) -> bool {
    tokens.iter().all(|tok| match tok {
        SearchToken::Namespace(v) => contains_case_insensitive(namespace, v),
        SearchToken::Node(v) => contains_case_insensitive(node_name, v),
        SearchToken::Status(v) => contains_case_insensitive(status, v),
        SearchToken::Ip(v) => contains_case_insensitive(pod_ip, v),
        SearchToken::Any(v) => {
            contains_case_insensitive(name, v)
                || contains_case_insensitive(namespace, v)
                || contains_case_insensitive(status, v)
                || contains_case_insensitive(node_name, v)
                || contains_case_insensitive(pod_ip, v)
                || contains_case_insensitive(detail_hint, v)
        }
    })
}

pub fn resource_matches_search_query(res: &ClusterResource, query: &str) -> bool {
    let tokens = parse_search_tokens(query);
    resource_matches_tokens(res, &tokens)
}

fn resource_matches_tokens(res: &ClusterResource, tokens: &[SearchToken]) -> bool {
    if tokens.is_empty() {
        return true;
    }
    tokens.iter().all(|tok| match tok {
        SearchToken::Namespace(v) => contains_case_insensitive(&res.namespace, v),
        SearchToken::Node(v) => contains_case_insensitive(&res.node_name, v),
        SearchToken::Status(v) => contains_case_insensitive(&res.status, v),
        SearchToken::Ip(v) => contains_case_insensitive(&res.pod_ip, v),
        SearchToken::Any(v) => {
            contains_case_insensitive(&res.name, v)
                || contains_case_insensitive(&res.namespace, v)
                || contains_case_insensitive(&res.status, v)
                || contains_case_insensitive(&res.detail, v)
                || contains_case_insensitive(&res.controlled_by_kind, v)
                || contains_case_insensitive(&res.controlled_by_name, v)
                || contains_case_insensitive(&res.pod_ip, v)
                || contains_case_insensitive(&res.node_name, v)
        }
    })
}

pub fn event_matches_search_query(ev: &ClusterEvent, query: &str) -> bool {
    let tokens = parse_search_tokens(query);
    event_matches_tokens(ev, &tokens)
}

fn event_matches_tokens(ev: &ClusterEvent, tokens: &[SearchToken]) -> bool {
    if tokens.is_empty() {
        return true;
    }
    tokens.iter().all(|tok| match tok {
        SearchToken::Namespace(v) => contains_case_insensitive(&ev.namespace, v),
        SearchToken::Status(v) => {
            contains_case_insensitive(&ev.event_type, v) || contains_case_insensitive(&ev.reason, v)
        }
        SearchToken::Node(v) | SearchToken::Ip(v) | SearchToken::Any(v) => {
            contains_case_insensitive(&ev.message, v)
                || contains_case_insensitive(&ev.reason, v)
                || contains_case_insensitive(&ev.namespace, v)
                || contains_case_insensitive(&ev.object, v)
                || contains_case_insensitive(&ev.kind, v)
                || contains_case_insensitive(&ev.event_type, v)
        }
    })
}

fn normalize_event_object_kind(raw_kind: &str) -> String {
    let base = raw_kind.split('.').next().unwrap_or(raw_kind);
    match base.to_ascii_lowercase().as_str() {
        "pod" => "Pod".into(),
        "node" => "Node".into(),
        "deployment" => "Deployment".into(),
        "replicaset" => "ReplicaSet".into(),
        "statefulset" => "StatefulSet".into(),
        "daemonset" => "DaemonSet".into(),
        "job" => "Job".into(),
        "cronjob" => "CronJob".into(),
        "service" => "Service".into(),
        "endpoints" => "Endpoints".into(),
        "ingress" => "Ingress".into(),
        "configmap" => "ConfigMap".into(),
        "secret" => "Secret".into(),
        "persistentvolumeclaim" => "PersistentVolumeClaim".into(),
        "persistentvolume" => "PersistentVolume".into(),
        "namespace" => "Namespace".into(),
        "serviceaccount" => "ServiceAccount".into(),
        "horizontalpodautoscaler" => "HorizontalPodAutoscaler".into(),
        _ => {
            let mut chars = base.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => "Object".into(),
            }
        }
    }
}

/// Parses a single `kubectl get events [-A] --no-headers` Server-Side Table line.
pub fn parse_event_table_line(
    line: &str,
    all_namespaces: bool,
    default_ns: &str,
) -> Option<ClusterEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with("No resources found") {
        return None;
    }
    let expected_prefix_cols = if all_namespaces { 5 } else { 4 };
    let mut iter = trimmed.split_whitespace();
    let namespace = if all_namespaces {
        clean_col(iter.next()?).to_owned()
    } else {
        default_ns.to_owned()
    };
    let last_seen = clean_col(iter.next()?).to_owned();
    let event_type = clean_col(iter.next()?).to_owned();
    let reason = clean_col(iter.next()?).to_owned();
    let obj_tok = iter.next()?;

    // Extract remainder of `trimmed` after `expected_prefix_cols` whitespace-delimited tokens.
    let mut rest = trimmed;
    for _ in 0..expected_prefix_cols {
        rest = rest.trim_start();
        let tok_len = rest.find(char::is_whitespace).unwrap_or(rest.len());
        rest = &rest[tok_len..];
    }
    let message = rest.trim().to_owned();

    let (kind, object) = if let Some((rk, name)) = obj_tok.split_once('/') {
        (normalize_event_object_kind(rk), name.to_owned())
    } else {
        ("Object".into(), obj_tok.to_owned())
    };

    Some(ClusterEvent {
        namespace,
        last_seen,
        event_type,
        reason,
        object,
        kind,
        message,
        count: 1,
    })
}

fn parse_secret_table_line(line: &str, default_ns: &str) -> Option<ClusterResource> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with("No resources found") {
        return None;
    }
    let cols: Vec<&str> = trimmed.split_whitespace().collect();
    if cols.len() >= 5 {
        let ns = clean_col(cols[0]);
        let name = cols[1];
        let sec_type = cols[2];
        let data_cnt = cols[3];
        let age = cols[4];
        Some(ClusterResource {
            kind: "Secret".into(),
            name: name.to_owned(),
            namespace: ns.to_owned(),
            status: sec_type.to_owned(),
            ready: format!("{data_cnt} keys"),
            age: age.to_owned(),
            detail: format!("type: {sec_type}"),
            ..Default::default()
        })
    } else if cols.len() == 4 {
        if cols[2].chars().all(|c| c.is_ascii_digit()) {
            let name = cols[0];
            let sec_type = cols[1];
            let data_cnt = cols[2];
            let age = cols[3];
            Some(ClusterResource {
                kind: "Secret".into(),
                name: name.to_owned(),
                namespace: default_ns.to_owned(),
                status: sec_type.to_owned(),
                ready: format!("{data_cnt} keys"),
                age: age.to_owned(),
                detail: format!("type: {sec_type}"),
                ..Default::default()
            })
        } else {
            let ns = clean_col(cols[0]);
            let name = cols[1];
            let sec_type = cols[2];
            let age = cols[3];
            Some(ClusterResource {
                kind: "Secret".into(),
                name: name.to_owned(),
                namespace: ns.to_owned(),
                status: sec_type.to_owned(),
                ready: "Secret".into(),
                age: age.to_owned(),
                detail: format!("type: {sec_type}"),
                ..Default::default()
            })
        }
    } else {
        None
    }
}

fn parse_cr_server_table_line(
    line: &str,
    crd_name: &str,
    all_namespaces: bool,
    default_ns: &str,
    is_cluster_scoped: bool,
) -> Option<ClusterResource> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with("No resources found") {
        return None;
    }
    let cols: Vec<&str> = trimmed.split_whitespace().collect();
    let namespaced_with_col = all_namespaces && !is_cluster_scoped;
    let min_cols = if namespaced_with_col { 2 } else { 1 };
    if cols.len() < min_cols {
        return None;
    }
    let (namespace, name, extra_start) = if namespaced_with_col {
        (clean_col(cols[0]).to_owned(), cols[1].to_owned(), 2usize)
    } else if is_cluster_scoped {
        (String::new(), cols[0].to_owned(), 1usize)
    } else {
        (default_ns.to_owned(), cols[0].to_owned(), 1usize)
    };
    let age = if cols.len() > extra_start {
        cols.last().copied().map(clean_col).unwrap_or("").to_owned()
    } else {
        String::new()
    };
    let middle_status = if cols.len() > extra_start + 1 {
        clean_col(cols[extra_start])
    } else {
        ""
    };
    let status = if middle_status.is_empty() {
        "Active".to_owned()
    } else {
        middle_status.to_owned()
    };
    Some(ClusterResource {
        kind: format!("CR:{crd_name}"),
        name,
        namespace,
        status,
        age,
        detail: crd_name.to_owned(),
        controlled_by_name: crd_name.to_owned(),
        ..Default::default()
    })
}

fn scan_pod_wide_lines_matching(
    text: &str,
    all_namespaces: bool,
    default_ns: &str,
    tokens: &[SearchToken],
    prev_metrics: &BTreeMap<(String, String), (String, String)>,
    outcome: &mut CategorySearchOutcome,
) {
    let min_cols = if all_namespaces { 6 } else { 5 };
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("No resources found") {
            continue;
        }
        let cols: Vec<&str> = trimmed.split_whitespace().collect();
        if cols.len() < min_cols {
            continue;
        }
        let mut idx = 0usize;
        let ns = if all_namespaces {
            let v = clean_col(cols[idx]);
            idx += 1;
            v
        } else {
            default_ns
        };
        let name = cols[idx];
        if name.starts_with("node-dbg-watch-") {
            continue;
        }
        idx += 1;
        let ready = cols[idx];
        idx += 1;
        let status_raw = cols[idx];
        if name.starts_with("node-debugger-")
            && matches!(status_raw, "Succeeded" | "Completed" | "Failed")
        {
            continue;
        }
        idx += 1;
        let restarts_tok = cols.get(idx).copied().unwrap_or("0");
        idx += 1;
        if cols.get(idx).is_some_and(|t| t.starts_with('(')) {
            while let Some(&tok) = cols.get(idx) {
                idx += 1;
                if tok.ends_with(')') {
                    break;
                }
            }
        }
        let age = cols.get(idx).copied().map(clean_col).unwrap_or("");
        let pod_ip = cols.get(idx + 1).copied().map(clean_col).unwrap_or("");
        let node_name = cols.get(idx + 2).copied().map(clean_col).unwrap_or("");

        outcome.scanned_total += 1;

        let detail_hint = format!("restarts: {restarts_tok}");
        if !pod_fields_match_tokens(
            tokens,
            ns,
            name,
            status_raw,
            pod_ip,
            node_name,
            &detail_hint,
        ) {
            continue;
        }

        outcome.total_matches += 1;
        if outcome.resources.len() >= MAX_SEARCH_RESULTS {
            outcome.truncated = true;
            continue;
        }

        let restarts = restarts_tok.parse::<u64>().unwrap_or(0);
        let (rdy_cnt, total_cnt) = ready
            .split_once('/')
            .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)))
            .unwrap_or((0, 0));
        let mut container_states = Vec::with_capacity(total_cnt.min(16));
        let mut containers = Vec::with_capacity(total_cnt.min(16));
        let is_done = matches!(status_raw, "Completed" | "Succeeded");
        for i in 0..total_cnt.min(16) {
            let cname = format!("c{}", i + 1);
            containers.push(cname.clone());
            let (cstate, cready, creason) = if is_done {
                ("terminated".to_owned(), false, "Completed".to_owned())
            } else if i < rdy_cnt {
                ("running".to_owned(), true, String::new())
            } else {
                ("waiting".to_owned(), false, status_raw.to_owned())
            };
            container_states.push(ContainerStateBadge {
                name: cname,
                state: cstate,
                ready: cready,
                reason: creason,
                ..Default::default()
            });
        }
        let detail = match (node_name.is_empty(), pod_ip.is_empty()) {
            (false, false) => format!("restarts: {restarts} • {pod_ip} on {node_name}"),
            (false, true) => format!("restarts: {restarts} • {node_name}"),
            _ => format!("restarts: {restarts}"),
        };
        let (cpu, memory) = prev_metrics
            .get(&(ns.to_owned(), name.to_owned()))
            .cloned()
            .unwrap_or_default();
        outcome.resources.push(ClusterResource {
            kind: "Pod".into(),
            name: name.to_owned(),
            namespace: ns.to_owned(),
            status: status_raw.to_owned(),
            ready: ready.to_owned(),
            age: age.to_owned(),
            detail,
            containers,
            container_states,
            cpu,
            memory,
            restarts,
            pod_ip: pod_ip.to_owned(),
            node_name: node_name.to_owned(),
            ..Default::default()
        });
    }
}

fn scan_generic_server_table_lines_matching(
    kind: &str,
    text: &str,
    all_namespaces: bool,
    default_ns: &str,
    tokens: &[SearchToken],
    outcome: &mut CategorySearchOutcome,
) {
    // Feed each line individually into `parse_server_table_resources` so the 25,000-row batch guard
    // never truncates large clusters while still reusing 100% of kind-specific column synthesis.
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("No resources found") {
            continue;
        }
        let mut parsed = parse_server_table_resources(kind, trimmed, all_namespaces, default_ns);
        let Some(res) = parsed.pop() else {
            continue;
        };
        outcome.scanned_total += 1;
        if resource_matches_tokens(&res, tokens) {
            outcome.total_matches += 1;
            if outcome.resources.len() < MAX_SEARCH_RESULTS {
                outcome.resources.push(res);
            } else {
                outcome.truncated = true;
            }
        }
    }
}

fn run_kubectl_commands_batch(
    project: &Project,
    arg_sets: &[Vec<String>],
    timeout_secs: u32,
) -> Vec<Result<ProcessOutput, Error>> {
    if arg_sets.len() <= 1 {
        return arg_sets
            .iter()
            .map(|args| run_kubectl_timed(project, args, None, Some(timeout_secs)))
            .collect();
    }
    let mut spawned: Vec<Option<host::Child>> = Vec::with_capacity(arg_sets.len());
    for args in arg_sets {
        let mut with_timeout = args.clone();
        with_timeout.push(format!("--request-timeout={timeout_secs}s"));
        let cmd = host::Command::new("kubectl").args(with_timeout);
        spawned.push(host::start(&cmd, Some(project)).ok());
    }
    let wait_ms = timeout_secs.saturating_mul(1_000).max(1_000);
    spawned
        .into_iter()
        .zip(arg_sets.iter())
        .map(|(maybe_child, args)| {
            if let Some(child) = maybe_child {
                let mut stdout = String::new();
                let mut stderr = String::new();
                let mut status: Option<i32> = None;
                let mut overflowed_ring = false;
                for _ in 0..8 {
                    match child.wait(wait_ms) {
                        Ok(chunk) => {
                            if stdout.len().saturating_add(chunk.stdout.len()) >= 900 * 1024 {
                                overflowed_ring = true;
                                break;
                            }
                            stdout.push_str(&chunk.stdout);
                            stderr.push_str(&chunk.stderr);
                            if chunk.status.is_some() {
                                status = chunk.status;
                                break;
                            }
                        }
                        Err(_) => {
                            overflowed_ring = true;
                            break;
                        }
                    }
                }
                let _ = child.kill();
                if !overflowed_ring && status.is_some() {
                    return Ok(ProcessOutput {
                        status,
                        stdout,
                        stderr,
                    });
                }
            }
            run_kubectl_timed(project, args, None, Some(timeout_secs))
        })
        .collect()
}

const MULTI_NS_SEARCH_CONCURRENCY: usize = 4;

#[allow(clippy::too_many_arguments)]
fn search_resources_via_raw_pages(
    project: &Project,
    cfg: &KubeConfigState,
    kind: &str,
    endpoint: (&str, &str, bool),
    tokens: &[SearchToken],
    prev_pod_metrics: &BTreeMap<(String, String), (String, String)>,
    prev_node_metrics: &BTreeMap<String, (String, String)>,
    is_cancelled: &dyn Fn() -> bool,
    outcome: &mut CategorySearchOutcome,
) -> bool {
    let mut cursor = String::new();
    for _ in 0..500 {
        if is_cancelled() {
            break;
        }
        let opts = FetchOptions {
            limit: 500,
            continue_token: cursor.clone(),
            append: true,
            ..FetchOptions::default()
        };
        match fetch_raw_paged_resources(project, cfg, kind, endpoint, &opts, Some(15)) {
            RawPageOutcome::Page(page) => {
                for mut res in page.items {
                    outcome.scanned_total += 1;
                    if resource_matches_tokens(&res, tokens) {
                        outcome.total_matches += 1;
                        if outcome.resources.len() < MAX_SEARCH_RESULTS {
                            if res.kind == "Pod"
                                && let Some((c, m)) =
                                    prev_pod_metrics.get(&(res.namespace.clone(), res.name.clone()))
                            {
                                res.cpu = c.clone();
                                res.memory = m.clone();
                            } else if res.kind == "Node"
                                && let Some((c, m)) = prev_node_metrics.get(&res.name)
                            {
                                res.cpu = c.clone();
                                res.memory = m.clone();
                            }
                            outcome.resources.push(res);
                        } else {
                            outcome.truncated = true;
                        }
                    }
                }
                if page.continue_token.is_empty() || page.continue_token == cursor {
                    return true;
                }
                cursor = page.continue_token;
            }
            RawPageOutcome::Unsupported | RawPageOutcome::Failed(_) => {
                return outcome.scanned_total > 0;
            }
        }
    }
    outcome.scanned_total > 0
}

fn search_events_via_raw_pages(
    project: &Project,
    cfg: &KubeConfigState,
    tokens: &[SearchToken],
    is_cancelled: &dyn Fn() -> bool,
    outcome: &mut CategorySearchOutcome,
) -> bool {
    let mut cursor = String::new();
    for _ in 0..500 {
        if is_cancelled() {
            break;
        }
        let opts = FetchOptions {
            limit: 500,
            continue_token: cursor.clone(),
            append: true,
            ..FetchOptions::default()
        };
        match fetch_raw_paged_events(project, cfg, &opts, Some(15)) {
            RawPageOutcome::Page(page) => {
                for ev in page.items {
                    outcome.scanned_total += 1;
                    if event_matches_tokens(&ev, tokens) {
                        outcome.total_matches += 1;
                        if outcome.events.len() < MAX_SEARCH_RESULTS {
                            outcome.events.push(ev);
                        } else {
                            outcome.truncated = true;
                        }
                    }
                }
                if page.continue_token.is_empty() || page.continue_token == cursor {
                    return true;
                }
                cursor = page.continue_token;
            }
            RawPageOutcome::Unsupported | RawPageOutcome::Failed(_) => {
                return outcome.scanned_total > 0;
            }
        }
    }
    outcome.scanned_total > 0
}

const K8S_TABLE_ACCEPT: &str = "application/json;as=Table;v=v1;g=meta.k8s.io,application/json;as=Table;v=v1beta1;g=meta.k8s.io";
const METADATA_TABLE_MAX_EST_ITEMS: u64 = 40_000;

#[derive(Clone, Debug, Default, Deserialize)]
struct TableColDefWire {
    #[serde(default)]
    name: String,
    #[serde(default)]
    priority: i32,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct TableOwnerRefWire {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    name: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct TableObjectMetaWire {
    #[serde(default)]
    namespace: String,
    #[serde(default, rename = "ownerReferences")]
    owner_references: Vec<TableOwnerRefWire>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct TableObjectWire {
    #[serde(default)]
    metadata: TableObjectMetaWire,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct TableRowWire {
    #[serde(default)]
    cells: Vec<Value>,
    #[serde(default)]
    object: Option<TableObjectWire>,
}

fn cell_cow(cells: &[Value], idx: usize) -> Cow<'_, str> {
    match cells.get(idx) {
        Some(Value::String(s)) => Cow::Borrowed(s.as_str()),
        Some(Value::Number(n)) => Cow::Owned(n.to_string()),
        Some(Value::Bool(b)) => Cow::Borrowed(if *b { "true" } else { "false" }),
        _ => Cow::Borrowed(""),
    }
}

fn clean_cell_cow(cells: &[Value], idx: usize) -> Cow<'_, str> {
    let v = cell_cow(cells, idx);
    if matches!(v.as_ref(), "<none>" | "<unknown>") {
        Cow::Borrowed("")
    } else {
        v
    }
}

fn parse_leading_u64(raw: &str) -> u64 {
    let tok = raw.split_whitespace().next().unwrap_or("0");
    tok.parse::<u64>().unwrap_or(0)
}

/// Streams a Kubernetes `meta.k8s.io/v1` `Table` response (raw JSON or gzip-compressed bytes)
/// row-by-row via `serde::de::Visitor`, avoiding allocating an 88k-row `Vec` or a full decompressed buffer.
fn stream_k8s_table_rows<F>(raw_bytes: &[u8], mut on_row: F) -> Result<(), String>
where
    F: FnMut(&[TableColDefWire], TableRowWire),
{
    struct RowsSeqVisitor<'a, CB> {
        cols: &'a [TableColDefWire],
        on_row: &'a mut CB,
    }

    impl<'de, CB> serde::de::Visitor<'de> for RowsSeqVisitor<'_, CB>
    where
        CB: FnMut(&[TableColDefWire], TableRowWire),
    {
        type Value = ();

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a Kubernetes Table rows array")
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            while let Some(row) = seq.next_element::<TableRowWire>()? {
                (self.on_row)(self.cols, row);
            }
            Ok(())
        }
    }

    struct RowsSeed<'a, CB> {
        cols: &'a [TableColDefWire],
        on_row: &'a mut CB,
    }

    impl<'de, CB> serde::de::DeserializeSeed<'de> for RowsSeed<'_, CB>
    where
        CB: FnMut(&[TableColDefWire], TableRowWire),
    {
        type Value = ();

        fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_seq(RowsSeqVisitor {
                cols: self.cols,
                on_row: self.on_row,
            })
        }
    }

    struct TableDocVisitor<'a, CB> {
        on_row: &'a mut CB,
    }

    impl<'de, CB> serde::de::Visitor<'de> for TableDocVisitor<'_, CB>
    where
        CB: FnMut(&[TableColDefWire], TableRowWire),
    {
        type Value = bool;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a Kubernetes Table object")
        }

        fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
        where
            M: serde::de::MapAccess<'de>,
        {
            let mut cols: Vec<TableColDefWire> = Vec::new();
            let mut saw_table = false;
            while let Some(key) = map.next_key::<Cow<'de, str>>()? {
                match key.as_ref() {
                    "kind" => {
                        let k: String = map.next_value()?;
                        if k == "Table" {
                            saw_table = true;
                        }
                    }
                    "columnDefinitions" => {
                        cols = map.next_value()?;
                        saw_table = true;
                    }
                    "rows" => {
                        saw_table = true;
                        map.next_value_seed(RowsSeed {
                            cols: &cols,
                            on_row: self.on_row,
                        })?;
                    }
                    _ => {
                        let _: serde::de::IgnoredAny = map.next_value()?;
                    }
                }
            }
            Ok(saw_table)
        }
    }

    use serde::Deserializer as _;
    let saw_table = if raw_bytes.starts_with(&[0x1f, 0x8b]) {
        let gz = flate2::read::GzDecoder::new(raw_bytes).take(96 * 1024 * 1024);
        let reader = BufReader::with_capacity(64 * 1024, gz);
        let mut de = serde_json::Deserializer::from_reader(reader);
        de.deserialize_map(TableDocVisitor {
            on_row: &mut on_row,
        })
        .map_err(|e| e.to_string())?
    } else {
        let mut de = serde_json::Deserializer::from_slice(raw_bytes);
        de.deserialize_map(TableDocVisitor {
            on_row: &mut on_row,
        })
        .map_err(|e| e.to_string())?
    };

    if saw_table {
        Ok(())
    } else {
        Err("response was not a Kubernetes Table".into())
    }
}

fn proxy_fetch_table_bytes(
    watches: &mut WatchManager,
    port: u16,
    uri: &str,
    timeout_ms: u32,
) -> Result<Vec<u8>, Error> {
    let url = format!("http://127.0.0.1:{port}{uri}");
    let mut req = HttpRequest::get(&url)
        .header("Accept", K8S_TABLE_ACCEPT)
        .header("Accept-Encoding", "gzip");
    req.timeout_ms = Some(timeout_ms);
    match host::fetch(&req) {
        Ok(resp) if resp.status == 200 => Ok(resp.body),
        Ok(resp) => Err(Error::Failed(format!(
            "HTTP {} from kubectl proxy for {uri}",
            resp.status
        ))),
        Err(err) => {
            let msg = err.to_string();
            if msg.contains("connect") || msg.contains("refused") || msg.contains("closed") {
                watches.stop_proxy();
            }
            Err(err)
        }
    }
}

fn table_row_to_event(row: &TableRowWire, default_ns: &str) -> Option<ClusterEvent> {
    if row.cells.len() < 4 {
        return None;
    }
    let ns = row
        .object
        .as_ref()
        .map(|o| o.metadata.namespace.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(default_ns)
        .to_owned();
    let last_seen = clean_col(&cell_cow(&row.cells, 0)).to_owned();
    let event_type = clean_col(&cell_cow(&row.cells, 1)).to_owned();
    let reason = clean_col(&cell_cow(&row.cells, 2)).to_owned();
    let obj_tok = cell_cow(&row.cells, 3);
    let message = if row.cells.len() >= 7 {
        cell_cow(&row.cells, 6).trim().to_owned()
    } else {
        cell_cow(&row.cells, 4).trim().to_owned()
    };
    let count = if row.cells.len() >= 9 {
        parse_leading_u64(&cell_cow(&row.cells, 8)).max(1)
    } else {
        1
    };
    let (kind, object) = if let Some((rk, name)) = obj_tok.split_once('/') {
        (normalize_event_object_kind(rk), name.to_owned())
    } else {
        ("Object".into(), obj_tok.into_owned())
    };
    Some(ClusterEvent {
        namespace: ns,
        last_seen,
        event_type,
        reason,
        object,
        kind,
        message,
        count,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_pod_resource_from_table_parts(
    ns: &str,
    name: &str,
    ready: &str,
    status_raw: &str,
    restarts_raw: &str,
    age: &str,
    pod_ip: &str,
    node_name: &str,
    owner: Option<(&str, &str)>,
    prev_metrics: &BTreeMap<(String, String), (String, String)>,
) -> ClusterResource {
    let restarts = parse_leading_u64(restarts_raw);
    let (rdy_cnt, total_cnt) = ready
        .split_once('/')
        .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)))
        .unwrap_or((0, 0));
    let mut container_states = Vec::with_capacity(total_cnt.min(16));
    let mut containers = Vec::with_capacity(total_cnt.min(16));
    let is_done = matches!(status_raw, "Completed" | "Succeeded");
    for i in 0..total_cnt.min(16) {
        let cname = format!("c{}", i + 1);
        containers.push(cname.clone());
        let (cstate, cready, creason) = if is_done {
            ("terminated".to_owned(), false, "Completed".to_owned())
        } else if i < rdy_cnt {
            ("running".to_owned(), true, String::new())
        } else {
            ("waiting".to_owned(), false, status_raw.to_owned())
        };
        container_states.push(ContainerStateBadge {
            name: cname,
            state: cstate,
            ready: cready,
            reason: creason,
            ..Default::default()
        });
    }
    let detail = match (node_name.is_empty(), pod_ip.is_empty()) {
        (false, false) => format!("restarts: {restarts} • {pod_ip} on {node_name}"),
        (false, true) => format!("restarts: {restarts} • {node_name}"),
        _ => format!("restarts: {restarts}"),
    };
    let (cpu, memory) = if !ns.is_empty() {
        prev_metrics
            .get(&(ns.to_owned(), name.to_owned()))
            .cloned()
            .unwrap_or_default()
    } else {
        (String::new(), String::new())
    };
    let (controlled_by_kind, controlled_by_name) = owner
        .map(|(k, n)| (k.to_owned(), n.to_owned()))
        .unwrap_or_default();
    ClusterResource {
        kind: "Pod".into(),
        name: name.to_owned(),
        namespace: ns.to_owned(),
        status: status_raw.to_owned(),
        ready: ready.to_owned(),
        age: age.to_owned(),
        detail,
        containers,
        container_states,
        cpu,
        memory,
        restarts,
        controlled_by_kind,
        controlled_by_name,
        pod_ip: pod_ip.to_owned(),
        node_name: node_name.to_owned(),
        ..Default::default()
    }
}

fn table_row_to_resource(
    kind: &str,
    cols: &[TableColDefWire],
    row: &TableRowWire,
    default_ns: &str,
    prev_pod_metrics: &BTreeMap<(String, String), (String, String)>,
    prev_node_metrics: &BTreeMap<String, (String, String)>,
) -> Option<ClusterResource> {
    if row.cells.is_empty() {
        return None;
    }
    let ns = row
        .object
        .as_ref()
        .map(|o| o.metadata.namespace.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(default_ns);
    let owner = row
        .object
        .as_ref()
        .and_then(|o| o.metadata.owner_references.first())
        .map(|or| (or.kind.as_str(), or.name.as_str()));

    match kind {
        "Pod" => {
            if row.cells.len() < 5 {
                return None;
            }
            let name = cell_cow(&row.cells, 0);
            if name.starts_with("node-dbg-watch-") {
                return None;
            }
            let ready = cell_cow(&row.cells, 1);
            let status_raw = cell_cow(&row.cells, 2);
            if name.starts_with("node-debugger-")
                && matches!(status_raw.as_ref(), "Succeeded" | "Completed" | "Failed")
            {
                return None;
            }
            let restarts_raw = cell_cow(&row.cells, 3);
            let age = clean_cell_cow(&row.cells, 4);
            let pod_ip = clean_cell_cow(&row.cells, 5);
            let node_name = clean_cell_cow(&row.cells, 6);
            Some(build_pod_resource_from_table_parts(
                ns,
                &name,
                &ready,
                &status_raw,
                &restarts_raw,
                &age,
                &pod_ip,
                &node_name,
                owner,
                prev_pod_metrics,
            ))
        }
        "Node" => {
            let name = cell_cow(&row.cells, 0);
            let status_raw = cell_cow(&row.cells, 1);
            let roles = clean_cell_cow(&row.cells, 2);
            let age = clean_cell_cow(&row.cells, 3);
            let version = clean_cell_cow(&row.cells, 4);
            let internal_ip = clean_cell_cow(&row.cells, 5);
            let kernel = clean_cell_cow(&row.cells, 8);
            let schedulable = if status_raw.contains("SchedulingDisabled") {
                "False"
            } else {
                "True"
            };
            let (cpu, memory) = prev_node_metrics
                .get(name.as_ref())
                .cloned()
                .unwrap_or_default();
            Some(ClusterResource {
                kind: "Node".into(),
                name: name.into_owned(),
                status: status_raw.replace(',', ", "),
                ready: version.into_owned(),
                age: age.into_owned(),
                detail: internal_ip.clone().into_owned(),
                cpu,
                memory,
                pod_ip: internal_ip.into_owned(),
                host_ip: if roles == "agent" {
                    String::new()
                } else {
                    roles.into_owned()
                },
                node_name: kernel.into_owned(),
                qos: schedulable.to_owned(),
                ..Default::default()
            })
        }
        "Namespace" => {
            let name = cell_cow(&row.cells, 0);
            let status = cell_cow(&row.cells, 1);
            let age = clean_cell_cow(&row.cells, 2);
            Some(ClusterResource {
                kind: "Namespace".into(),
                name: name.into_owned(),
                status: if status.is_empty() {
                    "Active".into()
                } else {
                    status.into_owned()
                },
                age: age.into_owned(),
                ..Default::default()
            })
        }
        "Secret" => {
            let name = cell_cow(&row.cells, 0);
            let sec_type = cell_cow(&row.cells, 1);
            let data_cnt = cell_cow(&row.cells, 2);
            let age = clean_cell_cow(&row.cells, 3);
            let (controlled_by_kind, controlled_by_name) = owner
                .map(|(k, n)| (k.to_owned(), n.to_owned()))
                .unwrap_or_default();
            Some(ClusterResource {
                kind: "Secret".into(),
                name: name.into_owned(),
                namespace: ns.to_owned(),
                status: sec_type.clone().into_owned(),
                ready: format!("{data_cnt} keys"),
                age: age.into_owned(),
                detail: format!("type: {sec_type}"),
                controlled_by_kind,
                controlled_by_name,
                ..Default::default()
            })
        }
        crd if crd.starts_with("CR:") => {
            let crd_name = crd.strip_prefix("CR:").unwrap_or(crd);
            let name = cell_cow(&row.cells, 0).into_owned();
            let age_idx = cols
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case("Age"))
                .or_else(|| cols.iter().rposition(|c| c.priority == 0))
                .unwrap_or(row.cells.len().saturating_sub(1));
            let age = if age_idx > 0 {
                clean_col(&cell_cow(&row.cells, age_idx)).to_owned()
            } else {
                String::new()
            };
            let status_idx = cols.iter().position(|c| {
                matches!(
                    c.name.to_ascii_lowercase().as_str(),
                    "status" | "phase" | "state" | "ready"
                )
            });
            let status = status_idx
                .map(|i| clean_col(&cell_cow(&row.cells, i)).to_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "Active".into());
            let (owner_k, owner_n) = owner
                .map(|(k, n)| (k.to_owned(), n.to_owned()))
                .unwrap_or_else(|| (String::new(), crd_name.to_owned()));
            Some(ClusterResource {
                kind: crd.to_owned(),
                name,
                namespace: ns.to_owned(),
                status,
                age,
                detail: crd_name.to_owned(),
                controlled_by_kind: owner_k,
                controlled_by_name: if owner_n.is_empty() {
                    crd_name.to_owned()
                } else {
                    owner_n
                },
                ..Default::default()
            })
        }
        _ => {
            let std_tokens: Vec<String> = if cols.is_empty() {
                row.cells
                    .iter()
                    .enumerate()
                    .map(|(i, _)| {
                        let s = cell_cow(&row.cells, i);
                        if s.is_empty() {
                            "<none>".to_owned()
                        } else {
                            s.replace(' ', "")
                        }
                    })
                    .collect()
            } else {
                cols.iter()
                    .enumerate()
                    .filter(|(_, c)| c.priority == 0)
                    .map(|(i, _)| {
                        let s = cell_cow(&row.cells, i);
                        if s.is_empty() {
                            "<none>".to_owned()
                        } else {
                            s.replace(' ', "")
                        }
                    })
                    .collect()
            };
            let synthetic_line = std_tokens.join(" ");
            let mut parsed = parse_server_table_resources(kind, &synthetic_line, false, ns);
            let mut res = parsed.pop()?;
            if let Some((ok, on)) = owner {
                res.controlled_by_kind = ok.to_owned();
                res.controlled_by_name = on.to_owned();
            }
            Some(res)
        }
    }
}

fn resource_identity_without_ns(res: &ClusterResource) -> (String, String, String) {
    (
        res.name.clone(),
        res.pod_ip.clone(),
        if !res.node_name.is_empty() {
            res.node_name.clone()
        } else {
            res.age.clone()
        },
    )
}

fn event_identity_without_ns(ev: &ClusterEvent) -> (String, String, String) {
    (
        format!("{}/{}", ev.kind, ev.object),
        ev.reason.clone(),
        ev.message.clone(),
    )
}

#[allow(clippy::too_many_arguments)]
fn resolve_missing_resource_namespaces_via_proxy(
    watches: &mut WatchManager,
    port: u16,
    category: &str,
    api_prefix: &str,
    plural: &str,
    prev_pod_metrics: &BTreeMap<(String, String), (String, String)>,
    is_cancelled: &dyn Fn() -> bool,
    resources: &mut [ClusterResource],
) {
    let unresolved_indices: Vec<usize> = resources
        .iter()
        .enumerate()
        .filter_map(|(i, r)| r.namespace.is_empty().then_some(i))
        .collect();
    if unresolved_indices.is_empty() {
        return;
    }

    let mut distinct_names: Vec<String> = Vec::new();
    let mut seen_names = BTreeSet::new();
    let mut distinct_nodes: Vec<String> = Vec::new();
    let mut seen_nodes = BTreeSet::new();
    let mut all_non_running = true;

    for &idx in &unresolved_indices {
        let r = &resources[idx];
        if seen_names.insert(r.name.clone()) {
            distinct_names.push(r.name.clone());
        }
        if category == "Pod" {
            if !r.node_name.is_empty() && seen_nodes.insert(r.node_name.clone()) {
                distinct_nodes.push(r.node_name.clone());
            }
            if matches!(r.status.as_str(), "Running" | "Succeeded" | "Completed") {
                all_non_running = false;
            }
        }
    }

    let mut selector_uris: Vec<String> = Vec::new();
    if distinct_names.len() <= 10 {
        for name in &distinct_names {
            let sel = url_encode_component(&format!("metadata.name={name}"));
            selector_uris.push(format!(
                "{api_prefix}/{plural}?resourceVersion=0&fieldSelector={sel}&includeObject=Metadata"
            ));
        }
    } else if category == "Pod" && all_non_running {
        let sel = url_encode_component("status.phase!=Running,status.phase!=Succeeded");
        selector_uris.push(format!(
            "{api_prefix}/{plural}?resourceVersion=0&fieldSelector={sel}&includeObject=Metadata"
        ));
    } else if category == "Pod" && !distinct_nodes.is_empty() && distinct_nodes.len() <= 10 {
        for node in &distinct_nodes {
            let sel = url_encode_component(&format!("spec.nodeName={node}"));
            selector_uris.push(format!(
                "{api_prefix}/{plural}?resourceVersion=0&fieldSelector={sel}&includeObject=Metadata"
            ));
        }
    } else if category == "Pod" && !distinct_nodes.is_empty() {
        for node in distinct_nodes.iter().take(10) {
            let sel = url_encode_component(&format!("spec.nodeName={node}"));
            selector_uris.push(format!(
                "{api_prefix}/{plural}?resourceVersion=0&fieldSelector={sel}&includeObject=Metadata"
            ));
        }
    } else {
        for name in distinct_names.iter().take(10) {
            let sel = url_encode_component(&format!("metadata.name={name}"));
            selector_uris.push(format!(
                "{api_prefix}/{plural}?resourceVersion=0&fieldSelector={sel}&includeObject=Metadata"
            ));
        }
    }

    type ResolvedMeta = (String, String, String);
    let mut by_exact_key: HashMap<(String, String, String), Vec<ResolvedMeta>> = HashMap::new();
    let mut by_name_only: HashMap<String, Vec<ResolvedMeta>> = HashMap::new();
    let empty_node = BTreeMap::new();

    for uri in selector_uris {
        if is_cancelled() {
            break;
        }
        let Ok(bytes) = proxy_fetch_table_bytes(watches, port, &uri, 8_000) else {
            continue;
        };
        let _ = stream_k8s_table_rows(&bytes, |cols, row| {
            let Some(resolved) =
                table_row_to_resource(category, cols, &row, "", prev_pod_metrics, &empty_node)
            else {
                return;
            };
            if resolved.namespace.is_empty() {
                return;
            }
            let meta = (
                resolved.namespace.clone(),
                resolved.controlled_by_kind.clone(),
                resolved.controlled_by_name.clone(),
            );
            by_exact_key
                .entry(resource_identity_without_ns(&resolved))
                .or_default()
                .push(meta.clone());
            by_name_only.entry(resolved.name).or_default().push(meta);
        });
    }

    for idx in unresolved_indices {
        let key = resource_identity_without_ns(&resources[idx]);
        let found = by_exact_key
            .get_mut(&key)
            .and_then(|v| (!v.is_empty()).then(|| v.remove(0)))
            .or_else(|| {
                by_name_only
                    .get_mut(&resources[idx].name)
                    .and_then(|v| (!v.is_empty()).then(|| v.remove(0)))
            });
        if let Some((ns, ok, on)) = found {
            if category == "Pod"
                && let Some((c, m)) =
                    prev_pod_metrics.get(&(ns.clone(), resources[idx].name.clone()))
            {
                resources[idx].cpu = c.clone();
                resources[idx].memory = m.clone();
            }
            resources[idx].namespace = ns;
            if !ok.is_empty() {
                resources[idx].controlled_by_kind = ok;
            }
            if !on.is_empty() {
                resources[idx].controlled_by_name = on;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn search_via_watch_cache_table_proxy(
    project: &Project,
    watches: &mut WatchManager,
    effective_cfg: &KubeConfigState,
    snap: &mut ClusterSnapshot,
    category: &str,
    is_cluster_scoped: bool,
    tokens: &[SearchToken],
    prev_pod_metrics: &BTreeMap<(String, String), (String, String)>,
    prev_node_metrics: &BTreeMap<String, (String, String)>,
    is_cancelled: &dyn Fn() -> bool,
    outcome: &mut CategorySearchOutcome,
) -> bool {
    let endpoint = if category == "Event" {
        ("/api/v1".to_owned(), "events".to_owned(), false)
    } else if let Some(ep) = resolve_watch_endpoint(project, effective_cfg, snap, category) {
        ep
    } else {
        return false;
    };
    let (api_prefix, plural, _) = endpoint;

    let Some(port) = watches.ensure_api_proxy(project, effective_cfg) else {
        return false;
    };

    let scan_table_bytes = |bytes: &[u8],
                            default_ns: &str,
                            out: &mut CategorySearchOutcome,
                            mut dedup_res: Option<
        &mut HashMap<(String, String, String), usize>,
    >,
                            mut dedup_ev: Option<&mut HashMap<(String, String, String), usize>>|
     -> Result<u64, String> {
        let mut rows_seen = 0u64;
        stream_k8s_table_rows(bytes, |cols, row| {
            if category == "Event" {
                let Some(ev) = table_row_to_event(&row, default_ns) else {
                    return;
                };
                rows_seen += 1;
                if event_matches_tokens(&ev, tokens) {
                    if let Some(ref mut map) = dedup_ev {
                        let k = event_identity_without_ns(&ev);
                        if let Some(cnt) = map.get_mut(&k)
                            && *cnt > 0
                        {
                            *cnt -= 1;
                            return;
                        }
                    }
                    out.total_matches += 1;
                    if out.events.len() < MAX_SEARCH_RESULTS {
                        out.events.push(ev);
                    } else {
                        out.truncated = true;
                    }
                }
            } else if category == "Pod" {
                if row.cells.len() < 5 {
                    return;
                }
                let name = cell_cow(&row.cells, 0);
                if name.starts_with("node-dbg-watch-") {
                    return;
                }
                let ready = cell_cow(&row.cells, 1);
                let status_raw = cell_cow(&row.cells, 2);
                if name.starts_with("node-debugger-")
                    && matches!(status_raw.as_ref(), "Succeeded" | "Completed" | "Failed")
                {
                    return;
                }
                let restarts_raw = cell_cow(&row.cells, 3);
                let age = clean_cell_cow(&row.cells, 4);
                let pod_ip = clean_cell_cow(&row.cells, 5);
                let node_name = clean_cell_cow(&row.cells, 6);
                let ns = row
                    .object
                    .as_ref()
                    .map(|o| o.metadata.namespace.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(default_ns);
                let owner = row
                    .object
                    .as_ref()
                    .and_then(|o| o.metadata.owner_references.first())
                    .map(|or| (or.kind.as_str(), or.name.as_str()));

                rows_seen += 1;
                let restarts_num = parse_leading_u64(&restarts_raw);
                let detail_hint = format!("restarts: {restarts_num}");
                let owner_matches = owner.is_some_and(|(ok, on)| {
                    tokens.iter().any(|t| {
                        matches!(t, SearchToken::Any(v) if contains_case_insensitive(ok, v) || contains_case_insensitive(on, v))
                    })
                });
                let matched = if owner_matches {
                    let probe = build_pod_resource_from_table_parts(
                        ns,
                        &name,
                        &ready,
                        &status_raw,
                        &restarts_raw,
                        &age,
                        &pod_ip,
                        &node_name,
                        owner,
                        prev_pod_metrics,
                    );
                    resource_matches_tokens(&probe, tokens)
                } else {
                    pod_fields_match_tokens(
                        tokens,
                        ns,
                        &name,
                        &status_raw,
                        &pod_ip,
                        &node_name,
                        &detail_hint,
                    )
                };
                if !matched {
                    return;
                }
                if let Some(ref mut map) = dedup_res {
                    let k = (
                        name.clone().into_owned(),
                        pod_ip.clone().into_owned(),
                        if !node_name.is_empty() {
                            node_name.clone().into_owned()
                        } else {
                            age.clone().into_owned()
                        },
                    );
                    if let Some(cnt) = map.get_mut(&k)
                        && *cnt > 0
                    {
                        *cnt -= 1;
                        return;
                    }
                }
                out.total_matches += 1;
                if out.resources.len() < MAX_SEARCH_RESULTS {
                    out.resources.push(build_pod_resource_from_table_parts(
                        ns,
                        &name,
                        &ready,
                        &status_raw,
                        &restarts_raw,
                        &age,
                        &pod_ip,
                        &node_name,
                        owner,
                        prev_pod_metrics,
                    ));
                } else {
                    out.truncated = true;
                }
            } else {
                let Some(res) = table_row_to_resource(
                    category,
                    cols,
                    &row,
                    default_ns,
                    prev_pod_metrics,
                    prev_node_metrics,
                ) else {
                    return;
                };
                rows_seen += 1;
                if resource_matches_tokens(&res, tokens) {
                    if let Some(ref mut map) = dedup_res {
                        let k = resource_identity_without_ns(&res);
                        if let Some(cnt) = map.get_mut(&k)
                            && *cnt > 0
                        {
                            *cnt -= 1;
                            return;
                        }
                    }
                    out.total_matches += 1;
                    if out.resources.len() < MAX_SEARCH_RESULTS {
                        out.resources.push(res);
                    } else {
                        out.truncated = true;
                    }
                }
            }
        })?;
        Ok(rows_seen)
    };

    // 1. Cluster-scoped resources (`Node`, `Namespace`, `PersistentVolume`, `ClusterRole`, etc.)
    if is_cluster_scoped {
        let uri = format!("{api_prefix}/{plural}?resourceVersion=0&includeObject=None");
        if let Ok(bytes) = proxy_fetch_table_bytes(watches, port, &uri, 20_000)
            && let Ok(rows) = scan_table_bytes(&bytes, "", outcome, None, None)
        {
            outcome.scanned_total = rows;
            return true;
        }
        return false;
    }

    // 2. Single or multi-namespace scope: query each selected namespace's watch cache directly (`includeObject=Metadata`)
    if !effective_cfg.all_namespaces {
        let active = effective_cfg.active_namespaces();
        let mut any_ok = false;
        for ns in &active {
            if is_cancelled() {
                break;
            }
            let uri = format!(
                "{api_prefix}/namespaces/{ns}/{plural}?resourceVersion=0&includeObject=Metadata"
            );
            if let Ok(bytes) = proxy_fetch_table_bytes(watches, port, &uri, 15_000)
                && let Ok(rows) = scan_table_bytes(&bytes, ns, outcome, None, None)
            {
                any_ok = true;
                outcome.scanned_total += rows;
            }
        }
        return any_ok;
    }

    // 3. All Namespaces:
    // 3a. Explicit `ns:<subquery>` token narrows directly to matching namespaces in `snap.namespaces` (~0.2s)
    let explicit_ns_filters: Vec<&str> = tokens
        .iter()
        .filter_map(|t| match t {
            SearchToken::Namespace(v) => Some(v.as_str()),
            _ => None,
        })
        .collect();
    if !explicit_ns_filters.is_empty() && !snap.namespaces.is_empty() {
        let target_namespaces: Vec<String> = snap
            .namespaces
            .iter()
            .filter(|ns| {
                explicit_ns_filters
                    .iter()
                    .all(|pat| contains_case_insensitive(ns, pat))
            })
            .cloned()
            .collect();
        if target_namespaces.is_empty() {
            return true;
        }
        if target_namespaces.len() <= 24 {
            let mut any_ok = false;
            for ns in &target_namespaces {
                if is_cancelled() {
                    break;
                }
                let uri = format!(
                    "{api_prefix}/namespaces/{ns}/{plural}?resourceVersion=0&includeObject=Metadata"
                );
                if let Ok(bytes) = proxy_fetch_table_bytes(watches, port, &uri, 12_000)
                    && let Ok(rows) = scan_table_bytes(&bytes, ns, outcome, None, None)
                {
                    any_ok = true;
                    outcome.scanned_total += rows;
                }
            }
            if any_ok {
                return true;
            }
        }
    }

    let est_total = snap
        .page_info
        .get(category)
        .map(|pi| pi.loaded_count + pi.remaining_count.unwrap_or(0))
        .or_else(|| snap.overview_counts.get(category).copied())
        .unwrap_or(0);

    // 3b. Medium/large scopes up to 40,000 items (`Deployments`, `Secrets`, `ConfigMaps`, `Events`, `CR:*`):
    //     fetch `includeObject=Metadata` in a single gzipped watch-cache request so every row carries exact `namespace`.
    if est_total > 0 && est_total <= METADATA_TABLE_MAX_EST_ITEMS {
        let meta_uri = format!("{api_prefix}/{plural}?resourceVersion=0&includeObject=Metadata");
        if let Ok(bytes) = proxy_fetch_table_bytes(watches, port, &meta_uri, 22_000) {
            let mut trial = CategorySearchOutcome {
                category: outcome.category.clone(),
                query: outcome.query.clone(),
                ..Default::default()
            };
            if let Ok(rows) = scan_table_bytes(&bytes, "", &mut trial, None, None) {
                trial.scanned_total = rows;
                *outcome = trial;
                return true;
            }
        }
    }

    // 3c. Massive scopes (`> 40,000` items, e.g. 88k+ Pods on `falcon-phx-ga`):
    //     Step 1: Fan out to any namespaces in `snap.namespaces` matching query tokens (~0.2s, exact `namespace`).
    let mut dedup_res_without_ns: HashMap<(String, String, String), usize> = HashMap::new();
    let mut dedup_ev_without_ns: HashMap<(String, String, String), usize> = HashMap::new();
    if !snap.namespaces.is_empty() {
        let any_tokens: Vec<&str> = tokens
            .iter()
            .filter_map(|t| match t {
                SearchToken::Any(v) if !v.is_empty() => Some(v.as_str()),
                _ => None,
            })
            .collect();
        if !any_tokens.is_empty() {
            let matching_ns: Vec<String> = snap
                .namespaces
                .iter()
                .filter(|ns| {
                    any_tokens
                        .iter()
                        .any(|tok| contains_case_insensitive(ns, tok))
                })
                .cloned()
                .collect();
            if !matching_ns.is_empty() && matching_ns.len() <= 16 {
                for ns in &matching_ns {
                    if is_cancelled() {
                        break;
                    }
                    let ns_uri = format!(
                        "{api_prefix}/namespaces/{ns}/{plural}?resourceVersion=0&includeObject=Metadata"
                    );
                    let Ok(bytes) = proxy_fetch_table_bytes(watches, port, &ns_uri, 8_000) else {
                        continue;
                    };
                    let _ = stream_k8s_table_rows(&bytes, |cols, row| {
                        if category == "Event" {
                            let Some(ev) = table_row_to_event(&row, ns) else {
                                return;
                            };
                            let mut ev_no_ns = ev.clone();
                            ev_no_ns.namespace.clear();
                            if event_matches_tokens(&ev_no_ns, tokens) {
                                *dedup_ev_without_ns
                                    .entry(event_identity_without_ns(&ev))
                                    .or_insert(0) += 1;
                            }
                            if event_matches_tokens(&ev, tokens) {
                                outcome.total_matches += 1;
                                if outcome.events.len() < MAX_SEARCH_RESULTS {
                                    outcome.events.push(ev);
                                } else {
                                    outcome.truncated = true;
                                }
                            }
                        } else if let Some(res) = table_row_to_resource(
                            category,
                            cols,
                            &row,
                            ns,
                            prev_pod_metrics,
                            prev_node_metrics,
                        ) {
                            let mut res_no_ns = res.clone();
                            res_no_ns.namespace.clear();
                            if resource_matches_tokens(&res_no_ns, tokens) {
                                *dedup_res_without_ns
                                    .entry(resource_identity_without_ns(&res))
                                    .or_insert(0) += 1;
                            }
                            if resource_matches_tokens(&res, tokens) {
                                outcome.total_matches += 1;
                                if outcome.resources.len() < MAX_SEARCH_RESULTS {
                                    outcome.resources.push(res);
                                } else {
                                    outcome.truncated = true;
                                }
                            }
                        }
                    });
                }
            }
        }
    }

    if is_cancelled() {
        return outcome.total_matches > 0;
    }

    //     Step 2: Full-cluster `?resourceVersion=0&includeObject=None` watch-cache scan (~2.3s / 1.6 MiB gzipped for 88k pods).
    let none_uri = format!("{api_prefix}/{plural}?resourceVersion=0&includeObject=None");
    let Ok(bytes) = proxy_fetch_table_bytes(watches, port, &none_uri, 25_000) else {
        return false;
    };
    let Ok(total_rows) = scan_table_bytes(
        &bytes,
        "",
        outcome,
        Some(&mut dedup_res_without_ns),
        Some(&mut dedup_ev_without_ns),
    ) else {
        return false;
    };
    outcome.scanned_total = total_rows;

    //     Step 3: Resolve `metadata.namespace` + `ownerReferences` for any non-Event matched rows (~0.3s).
    if category != "Event" && !is_cancelled() {
        resolve_missing_resource_namespaces_via_proxy(
            watches,
            port,
            category,
            &api_prefix,
            &plural,
            prev_pod_metrics,
            is_cancelled,
            &mut outcome.resources,
        );
    }

    true
}

/// Searches Kubernetes ground truth across the entire active scope (`All Namespaces`, single namespace,
/// or multiple selected namespaces) when the local UI table holds only a partial paginated window.
pub fn search_cluster_category_ground_truth(
    project: &Project,
    watches: &mut WatchManager,
    cfg: &KubeConfigState,
    snap: &mut ClusterSnapshot,
    category: &str,
    query: &str,
    is_cancelled: &dyn Fn() -> bool,
) -> CategorySearchOutcome {
    let trimmed_query = query.trim();
    let mut outcome = CategorySearchOutcome {
        category: category.to_owned(),
        query: trimmed_query.to_owned(),
        ..Default::default()
    };
    if trimmed_query.is_empty() {
        return outcome;
    }

    let (_, is_builtin_cluster_scoped, _) = kubectl_resource_info(category);
    let is_cluster_scoped = if let Some(crd_name) = category.strip_prefix("CR:") {
        snap.resources
            .iter()
            .find(|r| r.kind == "CustomResourceDefinition" && r.name == crd_name)
            .is_some_and(|r| r.status.eq_ignore_ascii_case("Cluster"))
    } else {
        is_builtin_cluster_scoped || matches!(category, "Node" | "Namespace")
    };

    // Fast Path: `namespace/subquery` syntax (e.g. `dayan/brix` or `kube-system/`) narrows a 90k-item
    // cluster query directly to that namespace whenever the prefix matches a real cluster namespace.
    let mut effective_cfg = cfg.clone();
    let mut effective_query = trimmed_query;
    if !is_cluster_scoped
        && !matches!(category, "Event" | "Ingress" | "HelmRelease" | "Workspace")
        && let Some((ns_part, rest_part)) = trimmed_query.split_once('/')
    {
        let ns_candidate = ns_part.trim();
        if !ns_candidate.is_empty()
            && let Some(matched_ns) = snap
                .namespaces
                .iter()
                .find(|n| n.eq_ignore_ascii_case(ns_candidate))
                .cloned()
        {
            let allowed_in_scope =
                cfg.all_namespaces || cfg.active_namespaces().iter().any(|n| n == &matched_ns);
            if !allowed_in_scope {
                return outcome;
            }
            effective_cfg = cfg.with_single_namespace(&matched_ns);
            effective_query = rest_part.trim();
        }
    }

    let tokens = parse_search_tokens(effective_query);

    let prev_pod_metrics_for_proxy: BTreeMap<(String, String), (String, String)> =
        if category == "Pod" {
            snap.resources
                .iter()
                .filter(|r| r.kind == "Pod" && (!r.cpu.is_empty() || !r.memory.is_empty()))
                .map(|r| {
                    (
                        (r.namespace.clone(), r.name.clone()),
                        (r.cpu.clone(), r.memory.clone()),
                    )
                })
                .collect()
        } else {
            BTreeMap::new()
        };
    let prev_node_metrics_for_proxy: BTreeMap<String, (String, String)> = if category == "Node" {
        snap.resources
            .iter()
            .filter(|r| r.kind == "Node" && (!r.cpu.is_empty() || !r.memory.is_empty()))
            .map(|r| (r.name.clone(), (r.cpu.clone(), r.memory.clone())))
            .collect()
    } else {
        BTreeMap::new()
    };

    if search_via_watch_cache_table_proxy(
        project,
        watches,
        &effective_cfg,
        snap,
        category,
        is_cluster_scoped,
        &tokens,
        &prev_pod_metrics_for_proxy,
        &prev_node_metrics_for_proxy,
        is_cancelled,
        &mut outcome,
    ) {
        return outcome;
    }

    match category {
        "Event" => {
            if effective_cfg.is_multi_namespace() {
                let active = effective_cfg.active_namespaces();
                let mut ran_table = false;
                for chunk in active.chunks(MULTI_NS_SEARCH_CONCURRENCY) {
                    if is_cancelled() {
                        break;
                    }
                    let arg_sets: Vec<Vec<String>> = chunk
                        .iter()
                        .map(|ns| {
                            let sub = effective_cfg.with_single_namespace(ns);
                            let mut a = sub.base_args(true);
                            a.extend(["get".into(), "events".into(), "--no-headers".into()]);
                            a
                        })
                        .collect();
                    let results = run_kubectl_commands_batch(project, &arg_sets, 20);
                    for (ns, res) in chunk.iter().zip(results) {
                        if let Ok(out) = res
                            && out.status == Some(0)
                        {
                            ran_table = true;
                            for line in out.stdout.lines() {
                                if let Some(ev) = parse_event_table_line(line, false, ns) {
                                    outcome.scanned_total += 1;
                                    if event_matches_tokens(&ev, &tokens) {
                                        outcome.total_matches += 1;
                                        if outcome.events.len() < MAX_SEARCH_RESULTS {
                                            outcome.events.push(ev);
                                        } else {
                                            outcome.truncated = true;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if !ran_table {
                    let _ = search_events_via_raw_pages(
                        project,
                        &effective_cfg,
                        &tokens,
                        is_cancelled,
                        &mut outcome,
                    );
                }
            } else {
                let mut ev_args = effective_cfg.base_args(true);
                ev_args.extend(["get".into(), "events".into()]);
                effective_cfg.push_scope_flags(&mut ev_args);
                ev_args.push("--no-headers".into());
                match run_kubectl_timed(project, &ev_args, None, Some(25)) {
                    Ok(out) if out.status == Some(0) => {
                        for line in out.stdout.lines() {
                            if let Some(ev) = parse_event_table_line(
                                line,
                                effective_cfg.all_namespaces,
                                &effective_cfg.namespace,
                            ) {
                                outcome.scanned_total += 1;
                                if event_matches_tokens(&ev, &tokens) {
                                    outcome.total_matches += 1;
                                    if outcome.events.len() < MAX_SEARCH_RESULTS {
                                        outcome.events.push(ev);
                                    } else {
                                        outcome.truncated = true;
                                    }
                                }
                            }
                        }
                    }
                    _ => {
                        if !search_events_via_raw_pages(
                            project,
                            &effective_cfg,
                            &tokens,
                            is_cancelled,
                            &mut outcome,
                        ) {
                            outcome.error = Some("Cluster event search timed out or failed".into());
                        }
                    }
                }
            }
        }
        "Node" => {
            let prev_metrics: BTreeMap<String, (String, String)> = snap
                .resources
                .iter()
                .filter(|r| r.kind == "Node" && (!r.cpu.is_empty() || !r.memory.is_empty()))
                .map(|r| (r.name.clone(), (r.cpu.clone(), r.memory.clone())))
                .collect();
            let mut wide_args = effective_cfg.base_args(false);
            wide_args.extend([
                "get".into(),
                "nodes".into(),
                "-o".into(),
                "wide".into(),
                "--no-headers".into(),
            ]);
            match run_kubectl_timed(project, &wide_args, None, Some(20)) {
                Ok(out) if out.status == Some(0) => {
                    for res in parse_nodes_wide_table(&out.stdout, &prev_metrics) {
                        outcome.scanned_total += 1;
                        if resource_matches_tokens(&res, &tokens) {
                            outcome.total_matches += 1;
                            if outcome.resources.len() < MAX_SEARCH_RESULTS {
                                outcome.resources.push(res);
                            } else {
                                outcome.truncated = true;
                            }
                        }
                    }
                }
                _ => {
                    if let Some((api_prefix, plural, cs)) = k8s_api_endpoint_info("Node") {
                        let empty_pod = BTreeMap::new();
                        let _ = search_resources_via_raw_pages(
                            project,
                            &effective_cfg,
                            "Node",
                            (api_prefix, plural, cs),
                            &tokens,
                            &empty_pod,
                            &prev_metrics,
                            is_cancelled,
                            &mut outcome,
                        );
                    }
                }
            }
        }
        "Namespace" => {
            let mut ns_args = effective_cfg.base_args(false);
            ns_args.extend(["get".into(), "namespaces".into(), "--no-headers".into()]);
            match run_kubectl_timed(project, &ns_args, None, Some(20)) {
                Ok(out) if out.status == Some(0) => {
                    for line in out.stdout.lines() {
                        let cols: Vec<&str> = line.split_whitespace().collect();
                        let Some(&name) = cols.first() else {
                            continue;
                        };
                        if name == "No" && cols.get(1) == Some(&"resources") {
                            continue;
                        }
                        let res = ClusterResource {
                            kind: "Namespace".into(),
                            name: name.to_owned(),
                            status: cols.get(1).copied().unwrap_or("Active").to_owned(),
                            age: cols.get(2).copied().unwrap_or("").to_owned(),
                            ..Default::default()
                        };
                        outcome.scanned_total += 1;
                        if resource_matches_tokens(&res, &tokens) {
                            outcome.total_matches += 1;
                            if outcome.resources.len() < MAX_SEARCH_RESULTS {
                                outcome.resources.push(res);
                            } else {
                                outcome.truncated = true;
                            }
                        }
                    }
                }
                _ => {
                    if let Some((api_prefix, plural, cs)) = k8s_api_endpoint_info("Namespace") {
                        let empty_pod = BTreeMap::new();
                        let empty_node = BTreeMap::new();
                        let _ = search_resources_via_raw_pages(
                            project,
                            &effective_cfg,
                            "Namespace",
                            (api_prefix, plural, cs),
                            &tokens,
                            &empty_pod,
                            &empty_node,
                            is_cancelled,
                            &mut outcome,
                        );
                    }
                }
            }
        }
        "Secret" => {
            if effective_cfg.is_multi_namespace() {
                let active = effective_cfg.active_namespaces();
                let mut ran_table = false;
                for chunk in active.chunks(MULTI_NS_SEARCH_CONCURRENCY) {
                    if is_cancelled() {
                        break;
                    }
                    let arg_sets: Vec<Vec<String>> = chunk
                        .iter()
                        .map(|ns| {
                            let sub = effective_cfg.with_single_namespace(ns);
                            let mut a = sub.base_args(true);
                            a.extend(["get".into(), "secrets".into(), "--no-headers".into()]);
                            a
                        })
                        .collect();
                    let results = run_kubectl_commands_batch(project, &arg_sets, 20);
                    for (ns, res) in chunk.iter().zip(results) {
                        if let Ok(out) = res
                            && out.status == Some(0)
                        {
                            ran_table = true;
                            for line in out.stdout.lines() {
                                if let Some(sec) = parse_secret_table_line(line, ns) {
                                    outcome.scanned_total += 1;
                                    if resource_matches_tokens(&sec, &tokens) {
                                        outcome.total_matches += 1;
                                        if outcome.resources.len() < MAX_SEARCH_RESULTS {
                                            outcome.resources.push(sec);
                                        } else {
                                            outcome.truncated = true;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if !ran_table
                    && let Some((api_prefix, plural, cs)) = k8s_api_endpoint_info("Secret")
                {
                    let empty_pod = BTreeMap::new();
                    let empty_node = BTreeMap::new();
                    let _ = search_resources_via_raw_pages(
                        project,
                        &effective_cfg,
                        "Secret",
                        (api_prefix, plural, cs),
                        &tokens,
                        &empty_pod,
                        &empty_node,
                        is_cancelled,
                        &mut outcome,
                    );
                }
            } else {
                let mut sec_args = effective_cfg.base_args(true);
                sec_args.extend(["get".into(), "secrets".into()]);
                effective_cfg.push_scope_flags(&mut sec_args);
                sec_args.push("--no-headers".into());
                match run_kubectl_timed(project, &sec_args, None, Some(25)) {
                    Ok(out) if out.status == Some(0) => {
                        for line in out.stdout.lines() {
                            if let Some(sec) =
                                parse_secret_table_line(line, &effective_cfg.namespace)
                            {
                                outcome.scanned_total += 1;
                                if resource_matches_tokens(&sec, &tokens) {
                                    outcome.total_matches += 1;
                                    if outcome.resources.len() < MAX_SEARCH_RESULTS {
                                        outcome.resources.push(sec);
                                    } else {
                                        outcome.truncated = true;
                                    }
                                }
                            }
                        }
                    }
                    _ => {
                        if let Some((api_prefix, plural, cs)) = k8s_api_endpoint_info("Secret") {
                            let empty_pod = BTreeMap::new();
                            let empty_node = BTreeMap::new();
                            let _ = search_resources_via_raw_pages(
                                project,
                                &effective_cfg,
                                "Secret",
                                (api_prefix, plural, cs),
                                &tokens,
                                &empty_pod,
                                &empty_node,
                                is_cancelled,
                                &mut outcome,
                            );
                        }
                    }
                }
            }
        }
        kind if kind.starts_with("CR:") => {
            let crd_name = kind.strip_prefix("CR:").unwrap_or(kind);
            if !is_cluster_scoped && effective_cfg.is_multi_namespace() {
                let active = effective_cfg.active_namespaces();
                let mut ran_table = false;
                for chunk in active.chunks(MULTI_NS_SEARCH_CONCURRENCY) {
                    if is_cancelled() {
                        break;
                    }
                    let arg_sets: Vec<Vec<String>> = chunk
                        .iter()
                        .map(|ns| {
                            let sub = effective_cfg.with_single_namespace(ns);
                            let mut a = sub.base_args(true);
                            a.extend(["get".into(), crd_name.to_owned(), "--no-headers".into()]);
                            a
                        })
                        .collect();
                    let results = run_kubectl_commands_batch(project, &arg_sets, 20);
                    for (ns, res) in chunk.iter().zip(results) {
                        if let Ok(out) = res
                            && out.status == Some(0)
                        {
                            ran_table = true;
                            for line in out.stdout.lines() {
                                if let Some(cr) =
                                    parse_cr_server_table_line(line, crd_name, false, ns, false)
                                {
                                    outcome.scanned_total += 1;
                                    if resource_matches_tokens(&cr, &tokens) {
                                        outcome.total_matches += 1;
                                        if outcome.resources.len() < MAX_SEARCH_RESULTS {
                                            outcome.resources.push(cr);
                                        } else {
                                            outcome.truncated = true;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if !ran_table
                    && let Some((api_prefix, plural, cs)) =
                        resolve_crd_api_endpoint(project, &effective_cfg, snap, crd_name, Some(6))
                {
                    let empty_pod = BTreeMap::new();
                    let empty_node = BTreeMap::new();
                    let _ = search_resources_via_raw_pages(
                        project,
                        &effective_cfg,
                        kind,
                        (&api_prefix, &plural, cs),
                        &tokens,
                        &empty_pod,
                        &empty_node,
                        is_cancelled,
                        &mut outcome,
                    );
                }
            } else {
                let mut cr_args = effective_cfg.base_args(!is_cluster_scoped);
                cr_args.extend(["get".into(), crd_name.to_owned()]);
                if !is_cluster_scoped {
                    effective_cfg.push_scope_flags(&mut cr_args);
                }
                cr_args.push("--no-headers".into());
                match run_kubectl_timed(project, &cr_args, None, Some(25)) {
                    Ok(out) if out.status == Some(0) => {
                        for line in out.stdout.lines() {
                            if let Some(cr) = parse_cr_server_table_line(
                                line,
                                crd_name,
                                effective_cfg.all_namespaces,
                                &effective_cfg.namespace,
                                is_cluster_scoped,
                            ) {
                                outcome.scanned_total += 1;
                                if resource_matches_tokens(&cr, &tokens) {
                                    outcome.total_matches += 1;
                                    if outcome.resources.len() < MAX_SEARCH_RESULTS {
                                        outcome.resources.push(cr);
                                    } else {
                                        outcome.truncated = true;
                                    }
                                }
                            }
                        }
                    }
                    _ => {
                        if let Some((api_prefix, plural, cs)) = resolve_crd_api_endpoint(
                            project,
                            &effective_cfg,
                            snap,
                            crd_name,
                            Some(6),
                        ) {
                            let empty_pod = BTreeMap::new();
                            let empty_node = BTreeMap::new();
                            let _ = search_resources_via_raw_pages(
                                project,
                                &effective_cfg,
                                kind,
                                (&api_prefix, &plural, cs),
                                &tokens,
                                &empty_pod,
                                &empty_node,
                                is_cancelled,
                                &mut outcome,
                            );
                        }
                    }
                }
            }
        }
        kind => {
            let (kubectl_type, cluster_scoped, _) = kubectl_resource_info(kind);
            let prev_pod_metrics: BTreeMap<(String, String), (String, String)> = if kind == "Pod" {
                snap.resources
                    .iter()
                    .filter(|r| r.kind == "Pod" && (!r.cpu.is_empty() || !r.memory.is_empty()))
                    .map(|r| {
                        (
                            (r.namespace.clone(), r.name.clone()),
                            (r.cpu.clone(), r.memory.clone()),
                        )
                    })
                    .collect()
            } else {
                BTreeMap::new()
            };

            if !cluster_scoped && effective_cfg.is_multi_namespace() {
                let active = effective_cfg.active_namespaces();
                let mut ran_table = false;
                for chunk in active.chunks(MULTI_NS_SEARCH_CONCURRENCY) {
                    if is_cancelled() {
                        break;
                    }
                    let arg_sets: Vec<Vec<String>> = chunk
                        .iter()
                        .map(|ns| {
                            let sub = effective_cfg.with_single_namespace(ns);
                            let mut a = sub.base_args(true);
                            a.extend(["get".into(), kubectl_type.to_owned()]);
                            if kind == "Pod" {
                                a.extend(["-o".into(), "wide".into()]);
                            }
                            a.push("--no-headers".into());
                            a
                        })
                        .collect();
                    let results = run_kubectl_commands_batch(project, &arg_sets, 20);
                    for (ns, res) in chunk.iter().zip(results) {
                        if let Ok(out) = res
                            && out.status == Some(0)
                        {
                            ran_table = true;
                            if kind == "Pod" {
                                scan_pod_wide_lines_matching(
                                    &out.stdout,
                                    false,
                                    ns,
                                    &tokens,
                                    &prev_pod_metrics,
                                    &mut outcome,
                                );
                            } else {
                                scan_generic_server_table_lines_matching(
                                    kind,
                                    &out.stdout,
                                    false,
                                    ns,
                                    &tokens,
                                    &mut outcome,
                                );
                            }
                        }
                    }
                }
                if !ran_table && let Some((api_prefix, plural, cs)) = k8s_api_endpoint_info(kind) {
                    let empty_node = BTreeMap::new();
                    let _ = search_resources_via_raw_pages(
                        project,
                        &effective_cfg,
                        kind,
                        (api_prefix, plural, cs),
                        &tokens,
                        &prev_pod_metrics,
                        &empty_node,
                        is_cancelled,
                        &mut outcome,
                    );
                }
            } else {
                let mut args = effective_cfg.base_args(!cluster_scoped);
                args.extend(["get".into(), kubectl_type.to_owned()]);
                if !cluster_scoped {
                    effective_cfg.push_scope_flags(&mut args);
                }
                if kind == "Pod" {
                    args.extend(["-o".into(), "wide".into()]);
                }
                args.push("--no-headers".into());

                match run_kubectl_timed(project, &args, None, Some(25)) {
                    Ok(out) if out.status == Some(0) => {
                        if kind == "Pod" {
                            scan_pod_wide_lines_matching(
                                &out.stdout,
                                effective_cfg.all_namespaces,
                                &effective_cfg.namespace,
                                &tokens,
                                &prev_pod_metrics,
                                &mut outcome,
                            );
                        } else {
                            let use_all_ns_col = !cluster_scoped && effective_cfg.all_namespaces;
                            let default_ns = if cluster_scoped {
                                ""
                            } else {
                                effective_cfg.namespace.as_str()
                            };
                            scan_generic_server_table_lines_matching(
                                kind,
                                &out.stdout,
                                use_all_ns_col,
                                default_ns,
                                &tokens,
                                &mut outcome,
                            );
                        }
                    }
                    _ => {
                        let empty_node = BTreeMap::new();
                        let ok = if let Some((api_prefix, plural, cs)) = k8s_api_endpoint_info(kind)
                        {
                            search_resources_via_raw_pages(
                                project,
                                &effective_cfg,
                                kind,
                                (api_prefix, plural, cs),
                                &tokens,
                                &prev_pod_metrics,
                                &empty_node,
                                is_cancelled,
                                &mut outcome,
                            )
                        } else {
                            false
                        };
                        if !ok {
                            outcome.error = Some(format!(
                                "Cluster search for {kind} timed out or exceeded transfer limit"
                            ));
                        }
                    }
                }
            }
        }
    }

    outcome
}
