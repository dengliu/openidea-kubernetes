//! Kubernetes for OpenIDEA (`dengliu.kubernetes`):
//! Mirrors the JetBrains IntelliJ Kubernetes plugin across both cluster runtime
//! management and rich YAML / Helm / Kustomize editor intelligence:
//! - Cluster Explorer tool window (`kubernetes`): browse contexts, namespaces,
//!   Workloads (Pods, Deployments, StatefulSets, DaemonSets, Jobs, CronJobs),
//!   Network (Services, Ingresses), Config & Storage (ConfigMaps, Secrets, PVCs),
//!   Nodes, CRDs, Events, Helm Releases, and Workspace YAML manifests.
//! - Live cluster actions: Describe, YAML extraction, Pod container log viewer
//!   with level filter & pattern matching, Follow Logs in terminal, Interactive
//!   Pod Shell (`kubectl exec -it`), Port Forward (`kubectl port-forward`),
//!   Scale Replicas, Rollout Restart, Apply / Diff / Dry-Run / Delete manifests,
//!   Kustomize build preview, and Helm template rendering.
//! - Editor intelligence (`Providers`): schema & live-template completion,
//!   cluster + workspace ConfigMap/Secret/Service/PVC/ServiceAccount/CRD/Helm
//!   completion, cross-resource Go to Definition and gutter line markers,
//!   static + dry-run manifest inspections, and one-click Quick Fixes.

pub mod intelligence;
pub mod kubectl;
pub mod manifest;

use intelligence::{
    ProjectIndex, completions_at, definitions_at, inspect_document, line_markers_for,
    normalize_slashes, project_of,
};
use kubectl::{
    ActiveWatchStream, ClusterSnapshot, FetchOptions, KubeConfigState, ParsedWatchFrame,
    WatchManager, WatchSpec, apply_or_diff_manifest, build_kustomize, cordon_node,
    delete_cluster_resource_opts, drain_node, enforce_large_cluster_scope, exec_pod_command,
    extract_object_resource_version, fetch_category_into_snapshot, fetch_category_with_options,
    fetch_cluster_snapshot, fetch_helm_release_detail, fetch_kubeconfig_overview,
    fetch_namespaces_into_snapshot, fetch_nodes_into_snapshot, fetch_pod_logs,
    fetch_resource_extras, inspect_cluster_resource, inspect_cluster_resource_json,
    open_debug_terminal, open_exec_or_debug_terminal, open_exec_terminal,
    open_follow_logs_terminal, open_node_shell_terminal, open_port_forward_terminal,
    render_helm_template, resolve_project, resolve_watch_endpoint, rollout_restart, scale_workload,
    suspend_cronjob, trigger_cronjob, workspace_manifests_json,
};
use manifest::{parse_yaml, utf16_len};
use openidea_plugin::host::{self, storage};
use openidea_plugin::providers::{
    CodeAction, CompletionItem, CompletionKind, Document, LineMarker, Providers,
};
use openidea_plugin::views::{Body, Button, View, ViewEvent, Viewport, Views};
use openidea_plugin::{
    Context, Diagnostic, Error, Event, Invocation, Location, NotificationLevel, Plugin, Position,
    Project, Range, Severity, StatusItem, register_plugin,
};
use serde_json::{Value, json};

pub const VIEW_ID: &str = "kubernetes";
pub const STATUS_ID: &str = "context";
const PAGE_HTML: &str = include_str!("page.html");
/// Safe ceiling below OpenIDEA's 8 MiB `MAX_VIEW_MESSAGE` bridge limit.
const MAX_SAFE_VIEW_BYTES: usize = 6 * 1024 * 1024;
/// Maximum cached resources embedded in initial `Body::Html` so `PAGE_HTML` never exceeds `MAX_HTML` (4 MiB).
const MAX_INITIAL_HTML_RESOURCES: usize = 500;

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[derive(Clone, Debug, Default)]
pub struct ContentTabTarget {
    pub context: String,
    pub namespace: String,
    pub kind: String,
    pub name: String,
}

#[derive(Default)]
pub struct KubernetesPlugin {
    pub indices: std::collections::BTreeMap<String, ProjectIndex>,
    pub snapshot: Option<ClusterSnapshot>,
    pub content_tabs: std::collections::HashMap<String, ContentTabTarget>,
    pub watches: WatchManager,
}

impl KubernetesPlugin {
    fn project_index_key(project: Option<&Project>, fallback_doc_path: Option<&str>) -> String {
        if let Some(proj) = project
            && let Ok(r) = proj.root()
        {
            return normalize_slashes(&r).trim_end_matches('/').to_owned();
        }
        if let Some(p) = fallback_doc_path {
            return format!("standalone:{}", normalize_slashes(p));
        }
        String::new()
    }

    fn index_mut(
        &mut self,
        project: Option<&Project>,
        fallback_doc_path: Option<&str>,
    ) -> &mut ProjectIndex {
        let key = Self::project_index_key(project, fallback_doc_path);
        self.indices.entry(key).or_default()
    }

    fn index_for(
        &self,
        project: Option<&Project>,
        fallback_doc_path: Option<&str>,
    ) -> Option<&ProjectIndex> {
        let key = Self::project_index_key(project, fallback_doc_path);
        self.indices.get(&key)
    }

    fn invalidate_project_index(&mut self, project: Option<&Project>) {
        let key = Self::project_index_key(project, None);
        if let Some(idx) = self.indices.get_mut(&key) {
            idx.invalidate();
        }
    }
    fn resolve_content_tab_target(&self, key: &str) -> Option<ContentTabTarget> {
        if let Some(t) = self.content_tabs.get(key) {
            return Some(t.clone());
        }
        let rest = key.strip_prefix("k8s-yaml:")?;
        let mut parts = rest.splitn(4, '|');
        let context = parts.next()?.to_owned();
        let namespace = parts.next()?.to_owned();
        let kind = parts.next()?.to_owned();
        let name = parts.next()?.to_owned();
        if kind.is_empty() || name.is_empty() {
            return None;
        }
        Some(ContentTabTarget {
            context,
            namespace,
            kind,
            name,
        })
    }
    fn update_status_bar(&self, cfg: &KubeConfigState) {
        let ctx_name = if let Some(ref snap) = self.snapshot
            && !snap.current_context.is_empty()
        {
            snap.current_context.as_str()
        } else if !cfg.context.is_empty() {
            cfg.context.as_str()
        } else {
            "k8s"
        };
        let active_ns = cfg.active_namespaces();
        let ns_name = if cfg.all_namespaces {
            "all".to_owned()
        } else if active_ns.len() > 1 {
            format!("{} +{}", active_ns[0], active_ns.len() - 1)
        } else if let Some(ref snap) = self.snapshot
            && !snap.current_namespace.is_empty()
        {
            snap.current_namespace.clone()
        } else if !cfg.namespace.is_empty() {
            cfg.namespace.clone()
        } else {
            "default".to_owned()
        };
        let text = format!("k8s: {ctx_name}/{ns_name}");
        host::set_status_item(
            STATUS_ID,
            &StatusItem {
                text,
                tooltip: Some(format!(
                    "Kubernetes context: {ctx_name}, namespace: {ns_name} (click to switch context)"
                )),
                command: Some("switch-context".into()),
            },
        );
    }

    fn ensure_catalog_loaded(&mut self, project: &Project, cfg: &mut KubeConfigState) {
        let needs_catalog = self
            .snapshot
            .as_ref()
            .is_none_or(|s| s.cluster_catalog.is_empty() || s.contexts.is_empty());
        if !needs_catalog {
            if let Some(ref mut snap) = self.snapshot {
                if cfg.context.is_empty() && !snap.current_context.is_empty() {
                    cfg.context = snap.current_context.clone();
                    let _ = storage::set("context", &cfg.context);
                } else if !cfg.context.is_empty() {
                    snap.current_context = cfg.context.clone();
                }
                snap.current_namespace = cfg.namespace.clone();
                snap.all_namespaces = cfg.all_namespaces;
                snap.selected_namespaces = cfg.active_namespaces();
            }
            return;
        }
        let mut snap = self.snapshot.take().unwrap_or_else(|| ClusterSnapshot {
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
        });
        match fetch_kubeconfig_overview(project, cfg) {
            Ok((current_ctx, contexts, ctx_ns, catalog)) => {
                if cfg.context.is_empty() && !current_ctx.is_empty() {
                    cfg.context = current_ctx.clone();
                    let _ = storage::set("context", &cfg.context);
                }
                if cfg.namespace == "default"
                    && cfg.selected_namespaces.len() <= 1
                    && let Some(ns) = ctx_ns
                    && !ns.is_empty()
                {
                    let keep_all = cfg.all_namespaces;
                    cfg.set_namespaces([ns.as_str()]);
                    cfg.all_namespaces = keep_all;
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
                snap.all_namespaces = cfg.all_namespaces;
                snap.selected_namespaces = cfg.active_namespaces();
                for ns in cfg.active_namespaces() {
                    if !snap.namespaces.contains(&ns) {
                        snap.namespaces.push(ns);
                    }
                }
                snap.namespaces.sort();
                snap.namespaces.dedup();
            }
            Err(e) => {
                snap.error = Some(e.to_string());
            }
        }
        self.snapshot = Some(snap);
    }

    fn refresh_cluster(&mut self, project: &Project) -> ClusterSnapshot {
        let mut cfg = KubeConfigState::load();
        let snap = fetch_cluster_snapshot(project, &mut cfg);
        self.snapshot = Some(snap.clone());
        self.update_status_bar(&cfg);
        snap
    }

    fn stored_hotbar_items() -> Option<Vec<String>> {
        let raw = storage::get("hotbarItems").ok().flatten()?;
        serde_json::from_str::<Vec<String>>(&raw).ok()
    }

    fn state_payload(&mut self, ctx: &Invocation) -> Value {
        let project = resolve_project(ctx).ok();
        let root = project
            .as_ref()
            .and_then(|p| p.root().ok())
            .unwrap_or_default();
        self.index_mut(project.as_ref(), None)
            .ensure(project.as_ref(), None);
        if let Some(ref p) = project {
            self.publish_project_icons(p);
        }
        if self.snapshot.is_none()
            && let Some(ref p) = project
        {
            self.refresh_cluster(p);
        }
        let empty = ProjectIndex::default();
        let idx = self.index_for(project.as_ref(), None).unwrap_or(&empty);
        let ws = workspace_manifests_json(idx, &root);
        let cfg = KubeConfigState::load();
        json!({
            "op": "state",
            "snapshot": self.snapshot.clone().unwrap_or_default(),
            "workspace": ws,
            "debuggerImage": cfg.debugger_image,
            "hotbarItems": Self::stored_hotbar_items(),
        })
    }

    fn send_view_payload(mut payload: Value) {
        let mut serialized = payload.to_string();
        if serialized.len() > MAX_SAFE_VIEW_BYTES {
            let orig = payload
                .pointer("/snapshot/resources")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            while serialized.len() > MAX_SAFE_VIEW_BYTES {
                let Some(cur_len) = payload
                    .pointer("/snapshot/resources")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                else {
                    break;
                };
                if cur_len <= 500 {
                    break;
                }
                let next_len = ((cur_len * 2) / 3).max(500);
                if let Some(res_arr) = payload
                    .pointer_mut("/snapshot/resources")
                    .and_then(Value::as_array_mut)
                {
                    res_arr.truncate(next_len);
                }
                if let Some(snap_obj) = payload.get_mut("snapshot").and_then(Value::as_object_mut) {
                    snap_obj.insert(
                        "error".into(),
                        Value::String(format!(
                            "Showing first {next_len} of {orig} resources to stay within view message limits; filter by namespace or search to narrow."
                        )),
                    );
                }
                serialized = payload.to_string();
            }
        }
        if let Err(e) = host::post_view_message(VIEW_ID, &serialized) {
            let fallback = json!({
                "op": "detail",
                "text": "",
                "banner": format!("Failed to update Kubernetes view: {e}"),
                "error": true,
            });
            let _ = host::post_view_message(VIEW_ID, &fallback.to_string());
        }
    }

    fn post_state(&mut self, ctx: &Invocation) {
        let payload = self.state_payload(ctx);
        Self::send_view_payload(payload);
        let cfg = KubeConfigState::load();
        self.update_status_bar(&cfg);
    }

    fn post_snapshot(&mut self, needs_bg_sync: bool) {
        self.post_snapshot_meta(needs_bg_sync, None);
    }

    fn post_snapshot_meta(&mut self, needs_bg_sync: bool, poll_duration_ms: Option<u64>) {
        let cfg = KubeConfigState::load();
        let mut payload = json!({
            "op": "state",
            "snapshot": self.snapshot.clone().unwrap_or_default(),
            "needsBackgroundSync": needs_bg_sync,
            "debuggerImage": cfg.debugger_image,
            "hotbarItems": Self::stored_hotbar_items(),
        });
        if let Some(ms) = poll_duration_ms {
            payload["pollCompleted"] = Value::Bool(true);
            payload["pollDurationMs"] = Value::Number(ms.into());
        }
        Self::send_view_payload(payload);
        self.update_status_bar(&cfg);
    }

    fn post_detail(text: impl Into<String>, banner: Option<String>, is_error: bool) {
        let msg = json!({
            "op": "detail",
            "text": text.into(),
            "banner": banner,
            "error": is_error,
        });
        let _ = host::post_view_message(VIEW_ID, &msg.to_string());
    }

    /// Starts or replaces the active category `watch=true` stream when `snap.page_info[cat].watch_supported` is true,
    /// or stops any existing category watch when switching to a large (`> 1,000` item) paginated scope.
    fn sync_category_watch(&mut self, project: &Project, cfg: &KubeConfigState, cat: &str) {
        let Some(ref mut snap) = self.snapshot else {
            self.watches.stop_category();
            return;
        };
        let Some(pi) = snap.page_info.get(cat).cloned() else {
            self.watches.stop_category();
            return;
        };
        if !pi.watch_supported || pi.resource_version.is_empty() {
            self.watches.stop_category();
            return;
        }
        let Some((api_prefix, plural, cluster_scoped)) =
            resolve_watch_endpoint(project, cfg, snap, cat)
        else {
            self.watches.stop_category();
            return;
        };
        let active_ns = cfg.active_namespaces();
        let desired: Vec<(WatchSpec, String)> =
            if cluster_scoped || snap.all_namespaces || active_ns.len() <= 1 {
                let target_ns = if cluster_scoped || snap.all_namespaces {
                    String::new()
                } else {
                    active_ns
                        .first()
                        .cloned()
                        .unwrap_or_else(|| snap.current_namespace.clone())
                };
                vec![(
                    WatchSpec {
                        context: snap.current_context.clone(),
                        kubeconfig: cfg.kubeconfig.clone(),
                        namespace: target_ns,
                        all_namespaces: snap.all_namespaces,
                        kind: cat.to_owned(),
                        api_prefix,
                        plural,
                        cluster_scoped,
                        field_selector_name: None,
                    },
                    pi.resource_version,
                )]
            } else if active_ns.len() <= kubectl::MAX_MULTI_NAMESPACE_WATCHES {
                let mut specs = Vec::with_capacity(active_ns.len());
                for ns in &active_ns {
                    let Some(rv) = pi.namespace_versions.get(ns).filter(|v| !v.is_empty()) else {
                        self.watches.stop_category();
                        if let Some(entry) = snap.page_info.get_mut(cat) {
                            entry.watch_supported = false;
                        }
                        return;
                    };
                    specs.push((
                        WatchSpec {
                            context: snap.current_context.clone(),
                            kubeconfig: cfg.kubeconfig.clone(),
                            namespace: ns.clone(),
                            all_namespaces: false,
                            kind: cat.to_owned(),
                            api_prefix: api_prefix.clone(),
                            plural: plural.clone(),
                            cluster_scoped: false,
                            field_selector_name: None,
                        },
                        rv.clone(),
                    ));
                }
                specs
            } else {
                self.watches.stop_category();
                return;
            };

        if self.watches.category_streams.len() == desired.len()
            && self
                .watches
                .category_streams
                .iter()
                .zip(desired.iter())
                .all(|(cur, (spec, _))| &cur.spec == spec)
        {
            return;
        }

        self.watches.stop_category();
        let mut started_streams = Vec::with_capacity(desired.len());
        for (spec, rv) in desired {
            match ActiveWatchStream::start(project, cfg, spec, rv) {
                Ok(stream) => started_streams.push(stream),
                Err(_) => {
                    for s in started_streams {
                        s.stop();
                    }
                    if let Some(entry) = snap.page_info.get_mut(cat) {
                        entry.watch_supported = false;
                    }
                    return;
                }
            }
        }
        self.watches.category_streams = started_streams;
    }

    /// Starts or replaces the single-object `fieldSelector=metadata.name=<name>` watch for the open Drawer.
    fn sync_drawer_watch(
        &mut self,
        project: &Project,
        cfg: &KubeConfigState,
        kind: &str,
        namespace: &str,
        name: &str,
        resource_version: String,
    ) {
        if name.trim().is_empty() || kind == "HelmRelease" || kind == "Event" {
            self.watches.stop_drawer();
            return;
        }
        let Some(ref mut snap) = self.snapshot else {
            self.watches.stop_drawer();
            return;
        };
        let Some((api_prefix, plural, cluster_scoped)) =
            resolve_watch_endpoint(project, cfg, snap, kind)
        else {
            self.watches.stop_drawer();
            return;
        };
        let target_ns = if cluster_scoped {
            String::new()
        } else if !namespace.trim().is_empty() {
            namespace.trim().to_owned()
        } else {
            cfg.namespace.clone()
        };
        let spec = WatchSpec {
            context: snap.current_context.clone(),
            kubeconfig: cfg.kubeconfig.clone(),
            namespace: target_ns,
            all_namespaces: false,
            kind: kind.to_owned(),
            api_prefix,
            plural,
            cluster_scoped,
            field_selector_name: Some(name.trim().to_owned()),
        };
        if let Some(ref cur) = self.watches.drawer_stream
            && cur.spec == spec
        {
            return;
        }
        self.watches.stop_drawer();
        if let Ok(stream) = ActiveWatchStream::start(project, cfg, spec, resource_version) {
            self.watches.drawer_stream = Some(stream);
        }
    }

    /// Drains non-blocking frames from both active category and drawer watch streams, updates `self.snapshot`,
    /// and posts compact `{ "op": "watch-delta", ... }` updates to the web view.
    fn poll_active_watches(
        &mut self,
        project: &Project,
        cfg: &KubeConfigState,
        active_cat: &str,
        drawer_target: Option<(&str, &str, &str)>,
        refresh_metrics: bool,
    ) {
        if let Some((dk, dns, dname)) = drawer_target {
            let matches_existing = self.watches.drawer_stream.as_ref().is_some_and(|s| {
                s.spec.kind == dk
                    && s.spec.field_selector_name.as_deref() == Some(dname)
                    && (s.spec.cluster_scoped || s.spec.namespace == dns || dns.is_empty())
            });
            if !matches_existing {
                self.sync_drawer_watch(project, cfg, dk, dns, dname, String::new());
            }
        } else {
            self.watches.stop_drawer();
        }

        let mut upserts: Vec<kubectl::ClusterResource> = Vec::new();
        let mut deleted: Vec<Value> = Vec::new();
        let mut event_upserts: Vec<kubectl::ClusterEvent> = Vec::new();
        let mut drawer_json_update: Option<(String, String, String, String)> = None;
        let mut drawer_deleted: Option<(String, String, String)> = None;
        let mut category_resync_needed = false;

        // 1. Drain category watch streams if active for `active_cat`
        if self
            .watches
            .category_streams
            .iter()
            .any(|s| s.spec.kind != active_cat)
        {
            self.watches.stop_category();
        }
        let existing_streams = std::mem::take(&mut self.watches.category_streams);
        let mut retained_streams: Vec<ActiveWatchStream> =
            Vec::with_capacity(existing_streams.len());
        for mut stream in existing_streams {
            let (frames, ended, gone) = stream.drain_frames();
            if let Some(ref mut snap) = self.snapshot {
                for frame in frames {
                    match frame {
                        ParsedWatchFrame::ResourceUpsert { mut resource, .. } => {
                            if resource.kind == "Pod"
                                && (resource.name.starts_with("node-dbg-watch-")
                                    || (resource.name.starts_with("node-debugger-")
                                        && matches!(
                                            resource.status.as_str(),
                                            "Succeeded" | "Completed" | "Failed"
                                        )))
                            {
                                continue;
                            }
                            if let Some(existing) = snap.resources.iter_mut().find(|r| {
                                r.kind == resource.kind
                                    && r.namespace == resource.namespace
                                    && r.name == resource.name
                            }) {
                                if resource.cpu.is_empty() && !existing.cpu.is_empty() {
                                    resource.cpu = existing.cpu.clone();
                                }
                                if resource.memory.is_empty() && !existing.memory.is_empty() {
                                    resource.memory = existing.memory.clone();
                                }
                                *existing = (*resource).clone();
                            } else {
                                snap.resources.push((*resource).clone());
                            }
                            if resource.kind == "Namespace"
                                && !snap.namespaces.contains(&resource.name)
                            {
                                snap.namespaces.push(resource.name.clone());
                                snap.namespaces.sort();
                            }
                            upserts.push(*resource);
                        }
                        ParsedWatchFrame::ResourceDeleted {
                            kind,
                            namespace,
                            name,
                            ..
                        } => {
                            snap.resources.retain(|r| {
                                !(r.kind == kind && r.namespace == namespace && r.name == name)
                            });
                            if kind == "Namespace" {
                                snap.namespaces.retain(|n| n != &name);
                            }
                            deleted.push(json!({
                                "kind": kind,
                                "namespace": namespace,
                                "name": name,
                            }));
                        }
                        ParsedWatchFrame::EventUpsert { event, .. } => {
                            if let Some(existing) = snap.events.iter_mut().find(|e| {
                                e.namespace == event.namespace
                                    && e.object == event.object
                                    && e.reason == event.reason
                            }) {
                                *existing = event.clone();
                            } else {
                                if snap.events.len() >= kubectl::MAX_APPENDED_ITEMS {
                                    snap.events.remove(0);
                                }
                                snap.events.push(event.clone());
                            }
                            event_upserts.push(event);
                        }
                        ParsedWatchFrame::Bookmark { .. } => {}
                        ParsedWatchFrame::Expired { .. } => {
                            category_resync_needed = true;
                        }
                        ParsedWatchFrame::Ignored | ParsedWatchFrame::Malformed => {}
                    }
                }
                let cat_cnt = if active_cat == "Event" {
                    snap.events.len() as u64
                } else {
                    snap.resources
                        .iter()
                        .filter(|r| r.kind == active_cat)
                        .count() as u64
                };
                snap.overview_counts.insert(active_cat.to_owned(), cat_cnt);
                if let Some(pi) = snap.page_info.get_mut(active_cat) {
                    pi.loaded_count = cat_cnt;
                    if !stream.resource_version.is_empty() {
                        if !stream.spec.namespace.is_empty() && !pi.namespace_versions.is_empty() {
                            pi.namespace_versions.insert(
                                stream.spec.namespace.clone(),
                                stream.resource_version.clone(),
                            );
                        } else {
                            pi.resource_version = stream.resource_version.clone();
                        }
                    }
                }
            }

            if gone || category_resync_needed {
                stream.stop();
                category_resync_needed = true;
            } else if ended {
                let rv = stream.resource_version.clone();
                let spec = stream.spec.clone();
                stream.stop();
                if let Ok(reopened) = ActiveWatchStream::start(project, cfg, spec, rv) {
                    retained_streams.push(reopened);
                } else {
                    category_resync_needed = true;
                }
            } else {
                retained_streams.push(stream);
            }
        }
        if category_resync_needed {
            for s in retained_streams {
                s.stop();
            }
        } else {
            self.watches.category_streams = retained_streams;
        }

        // Optional lightweight `kubectl top pods` refresh on small watched Pod scopes
        if refresh_metrics
            && !category_resync_needed
            && active_cat == "Pod"
            && !self.watches.category_streams.is_empty()
            && let Some(ref mut snap) = self.snapshot
        {
            let pod_cnt = snap.resources.iter().filter(|r| r.kind == "Pod").count();
            if pod_cnt > 0 && pod_cnt <= 300 {
                kubectl::enrich_pod_metrics(project, cfg, snap);
                upserts = snap
                    .resources
                    .iter()
                    .filter(|r| r.kind == "Pod")
                    .cloned()
                    .collect();
            }
        }

        // 2. Drain single-resource Drawer watch stream
        if let Some(mut dstream) = self.watches.drawer_stream.take() {
            let (frames, ended, gone) = dstream.drain_frames();
            for frame in frames {
                match frame {
                    ParsedWatchFrame::ResourceUpsert {
                        resource,
                        raw_object_json,
                        ..
                    } => {
                        if let Some(ref mut snap) = self.snapshot
                            && let Some(existing) = snap.resources.iter_mut().find(|r| {
                                r.kind == resource.kind
                                    && r.namespace == resource.namespace
                                    && r.name == resource.name
                            })
                        {
                            let prev_cpu = existing.cpu.clone();
                            let prev_mem = existing.memory.clone();
                            *existing = (*resource).clone();
                            if existing.cpu.is_empty() {
                                existing.cpu = prev_cpu;
                            }
                            if existing.memory.is_empty() {
                                existing.memory = prev_mem;
                            }
                        }
                        if !upserts.iter().any(|u| {
                            u.kind == resource.kind
                                && u.namespace == resource.namespace
                                && u.name == resource.name
                        }) {
                            upserts.push((*resource).clone());
                        }
                        drawer_json_update = Some((
                            resource.kind.clone(),
                            resource.namespace.clone(),
                            resource.name.clone(),
                            raw_object_json,
                        ));
                    }
                    ParsedWatchFrame::ResourceDeleted {
                        kind,
                        namespace,
                        name,
                        ..
                    } => {
                        drawer_deleted = Some((kind, namespace, name));
                    }
                    _ => {}
                }
            }
            if gone || drawer_deleted.is_some() {
                dstream.stop();
            } else if ended {
                let rv = dstream.resource_version.clone();
                let spec = dstream.spec.clone();
                dstream.stop();
                if let Ok(reopened) = ActiveWatchStream::start(project, cfg, spec, rv) {
                    self.watches.drawer_stream = Some(reopened);
                }
            } else {
                self.watches.drawer_stream = Some(dstream);
            }
        }

        if category_resync_needed {
            if let Some(ref mut snap) = self.snapshot {
                let limit = snap
                    .page_info
                    .get(active_cat)
                    .map(|pi| pi.limit)
                    .unwrap_or(kubectl::DEFAULT_PAGE_LIMIT);
                fetch_category_with_options(
                    project,
                    cfg,
                    snap,
                    active_cat,
                    FetchOptions {
                        is_background_poll: true,
                        include_metrics: false,
                        limit,
                        ..FetchOptions::default()
                    },
                );
            }
            self.sync_category_watch(project, cfg, active_cat);
            self.post_snapshot_meta(false, Some(0));
            return;
        }

        let cat_watching = !self.watches.category_streams.is_empty();
        let drawer_watching = self.watches.drawer_stream.is_some();
        let mut msg = json!({
            "op": "watch-delta",
            "category": active_cat,
            "categoryWatching": cat_watching,
            "drawerWatching": drawer_watching,
            "upserts": upserts,
            "deleted": deleted,
            "eventUpserts": event_upserts,
        });
        if let Some((dk, dns, dname, raw_json)) = drawer_json_update {
            msg["drawerResource"] = json!({
                "kind": dk,
                "namespace": dns,
                "name": dname,
                "json": raw_json,
            });
        }
        if let Some((dk, dns, dname)) = drawer_deleted {
            msg["drawerDeleted"] = json!({
                "kind": dk,
                "namespace": dns,
                "name": dname,
            });
        }
        Self::send_view_payload(msg);
    }

    /// Runs static inspections (and optional `kubectl --dry-run=client`) on `path` and publishes diagnostics.
    pub fn check_and_publish(
        &mut self,
        path: &str,
        run_dry_run_override: bool,
    ) -> Result<usize, Error> {
        if !(path.ends_with(".yaml") || path.ends_with(".yml")) {
            return Ok(0);
        }
        let Some(project) = project_of(path) else {
            return Ok(0);
        };
        let Ok(text) = project.read_file(path) else {
            return Ok(0);
        };
        self.index_mut(Some(&project), Some(path))
            .ensure(Some(&project), Some((path, &text)));
        self.publish_project_icons(&project);
        let empty = ProjectIndex::default();
        let proj_index = self.index_for(Some(&project), Some(path)).unwrap_or(&empty);
        let parsed = proj_index
            .files
            .get(&normalize_slashes(path))
            .cloned()
            .unwrap_or_else(|| parse_yaml(&text));

        if parsed.marked_non_k8s || parsed.docs.is_empty() {
            host::publish_diagnostics(path, &[])?;
            return Ok(0);
        }

        let warn_latest = host::setting_bool("warnLatestTag").unwrap_or(true);
        let warn_resources = host::setting_bool("warnMissingResources").unwrap_or(true);
        let issues = inspect_document(
            proj_index,
            path,
            &text,
            &parsed,
            warn_latest,
            warn_resources,
        );
        let mut diagnostics: Vec<Diagnostic> = issues.into_iter().map(|i| i.diagnostic).collect();

        let dry_run = run_dry_run_override || host::setting_bool("dryRunOnSave").unwrap_or(false);
        if dry_run && !text.contains("{{") {
            let cfg = KubeConfigState::load();
            if let Err(e) = apply_or_diff_manifest(&project, &cfg, &text, "dry-run") {
                let first_line = text.lines().next().unwrap_or("");
                let range =
                    Range::new(Position::new(0, 0), Position::new(0, utf16_len(first_line)));
                diagnostics.push(intelligence::diag_with_source(
                    range,
                    Severity::Error,
                    e.to_string(),
                    "kubectl-dry-run",
                    "kubectl",
                    false,
                    false,
                ));
            }
        }

        let count = diagnostics.len();
        host::publish_diagnostics(path, &diagnostics)?;
        Ok(count)
    }

    fn scan_project_manifests(&mut self, ctx: &Invocation) -> Result<String, Error> {
        let project = resolve_project(ctx)?;
        let _task = host::begin_task("Kubernetes: Inspecting project manifests", true);
        let paths: Vec<String> = {
            let idx = self.index_mut(Some(&project), None);
            idx.invalidate();
            idx.ensure(Some(&project), None);
            idx.files.keys().cloned().collect()
        };
        let mut total_issues = 0usize;
        let mut total_files = 0usize;
        for path in paths {
            if let Ok(n) = self.check_and_publish(&path, false) {
                total_files += 1;
                total_issues += n;
            }
        }
        let summary =
            format!("Inspected {total_files} YAML file(s): {total_issues} issue(s) reported");
        host::notify(NotificationLevel::Info, "Kubernetes Inspection", &summary);
        Ok(summary)
    }

    fn handle_page_message(&mut self, raw: &str, ctx: &Invocation) -> Result<(), Error> {
        let v: Value = serde_json::from_str(raw).map_err(|e| Error::Invalid(e.to_string()))?;
        let op = v.get("op").and_then(Value::as_str).unwrap_or("");
        let _task = match op {
            "poll-category" | "poll-watch" | "stop-drawer-watch" => None,
            "search-category" => Some(host::begin_task("Kubernetes: Searching cluster…", true)),
            _ => Some(host::begin_task("Kubernetes", false)),
        };
        match op {
            "init" => {
                let project = resolve_project(ctx).ok();
                let root = project
                    .as_ref()
                    .and_then(|p| p.root().ok())
                    .unwrap_or_default();
                let mut cfg = KubeConfigState::load();
                if let Some(ref p) = project {
                    self.ensure_catalog_loaded(p, &mut cfg);
                }
                self.update_status_bar(&cfg);
                let snap = self.snapshot.clone().unwrap_or_default();
                self.index_mut(project.as_ref(), None)
                    .ensure(project.as_ref(), None);
                let empty = ProjectIndex::default();
                let idx = self.index_for(project.as_ref(), None).unwrap_or(&empty);
                let ws = workspace_manifests_json(idx, &root);
                let cat = v.get("category").and_then(Value::as_str).unwrap_or("Pod");
                let req_limit = v
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map(|n| (n as u32).clamp(10, 500))
                    .unwrap_or(kubectl::DEFAULT_PAGE_LIMIT);
                let needs_fetch =
                    snap.namespaces.len() <= 3 || !snap.resources.iter().any(|r| r.kind == cat);
                let payload = json!({
                    "op": "state",
                    "snapshot": snap,
                    "workspace": ws,
                    "needsClusterFetch": false,
                    "debuggerImage": cfg.debugger_image,
                    "hotbarItems": Self::stored_hotbar_items(),
                });
                Self::send_view_payload(payload);
                if needs_fetch
                    && let Some(ref p) = project
                    && let Some(ref mut s) = self.snapshot
                {
                    if s.namespaces.len() <= 3 {
                        fetch_namespaces_into_snapshot(p, &cfg, s);
                    }
                    enforce_large_cluster_scope(&mut cfg, s);
                    fetch_category_with_options(
                        p,
                        &cfg,
                        s,
                        cat,
                        FetchOptions {
                            limit: req_limit,
                            ..FetchOptions::default()
                        },
                    );
                }
                if let Some(ref p) = project {
                    self.sync_category_watch(p, &cfg, cat);
                    if needs_fetch {
                        self.post_snapshot(false);
                    }
                }
            }
            "refresh" => {
                let project = resolve_project(ctx)?;
                let req_limit = v
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map(|n| (n as u32).clamp(10, 500))
                    .unwrap_or(kubectl::DEFAULT_PAGE_LIMIT);
                if let Some(cat) = v.get("category").and_then(Value::as_str) {
                    if cat == "Workspace" {
                        self.watches.stop_category();
                        let idx = self.index_mut(Some(&project), None);
                        idx.invalidate();
                        idx.ensure(Some(&project), None);
                        self.post_state(ctx);
                        return Ok(());
                    }
                    let mut cfg = KubeConfigState::load();
                    self.ensure_catalog_loaded(&project, &mut cfg);
                    self.watches.stop_category();
                    self.watches.stop_proxy();
                    if let Some(ref mut snap) = self.snapshot {
                        fetch_namespaces_into_snapshot(&project, &cfg, snap);
                        fetch_category_with_options(
                            &project,
                            &cfg,
                            snap,
                            cat,
                            FetchOptions {
                                limit: req_limit,
                                ..FetchOptions::default()
                            },
                        );
                    }
                    self.sync_category_watch(&project, &cfg, cat);
                    self.post_snapshot(false);
                } else {
                    self.watches.stop_all();
                    self.invalidate_project_index(Some(&project));
                    self.refresh_cluster(&project);
                    self.post_state(ctx);
                }
            }
            "set-hotbar" => {
                if let Some(items) = v.get("items").and_then(Value::as_array) {
                    let names: Vec<String> = items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect();
                    if let Ok(serialized) = serde_json::to_string(&names) {
                        let _ = storage::set("hotbarItems", &serialized);
                    }
                }
            }
            "set-context" => {
                self.watches.stop_all();
                if let Some(c) = v.get("context").and_then(Value::as_str) {
                    if let Ok(project) = resolve_project(ctx) {
                        let mut cfg = KubeConfigState::load();
                        self.ensure_catalog_loaded(&project, &mut cfg);
                    }
                    let prev_catalog = self
                        .snapshot
                        .as_ref()
                        .map(|s| s.cluster_catalog.clone())
                        .unwrap_or_default();
                    let prev_contexts = self
                        .snapshot
                        .as_ref()
                        .map(|s| s.contexts.clone())
                        .unwrap_or_default();
                    let default_ns = prev_catalog
                        .iter()
                        .find(|item| item.name == c && !item.namespace.is_empty())
                        .map(|item| item.namespace.clone())
                        .unwrap_or_else(|| "default".into());
                    let _ = storage::set("context", c);
                    let mut cfg = KubeConfigState::load();
                    cfg.context = c.to_owned();
                    cfg.set_namespaces([default_ns.as_str()]);
                    cfg.persist_scope();

                    if let Some(cat) = v.get("category").and_then(Value::as_str) {
                        let req_limit = v
                            .get("limit")
                            .and_then(Value::as_u64)
                            .map(|n| (n as u32).clamp(10, 500))
                            .unwrap_or(kubectl::DEFAULT_PAGE_LIMIT);
                        let mut snap = ClusterSnapshot {
                            connected: false,
                            error: None,
                            current_context: c.to_owned(),
                            contexts: prev_contexts,
                            cluster_catalog: prev_catalog,
                            current_namespace: default_ns.clone(),
                            all_namespaces: false,
                            selected_namespaces: vec![default_ns.clone()],
                            namespaces: vec![default_ns, "default".into(), "kube-system".into()],
                            resources: Vec::new(),
                            events: Vec::new(),
                            helm_releases: Vec::new(),
                            ..Default::default()
                        };
                        snap.namespaces.sort();
                        snap.namespaces.dedup();
                        self.update_status_bar(&cfg);
                        if let Ok(project) = resolve_project(ctx) {
                            fetch_namespaces_into_snapshot(&project, &cfg, &mut snap);
                            if cat != "ClusterCatalog" && cat != "Workspace" {
                                fetch_category_with_options(
                                    &project,
                                    &cfg,
                                    &mut snap,
                                    cat,
                                    FetchOptions {
                                        limit: req_limit,
                                        ..FetchOptions::default()
                                    },
                                );
                            }
                            self.snapshot = Some(snap);
                            if cat != "ClusterCatalog" && cat != "Workspace" {
                                self.sync_category_watch(&project, &cfg, cat);
                            }
                            self.post_snapshot(false);
                        } else {
                            self.snapshot = Some(snap);
                            self.post_snapshot(false);
                        }
                    } else {
                        if let Ok(project) = resolve_project(ctx) {
                            self.refresh_cluster(&project);
                        }
                        self.post_state(ctx);
                    }
                }
            }
            "sync-cluster" => {
                let project = resolve_project(ctx)?;
                let mut cfg = KubeConfigState::load();
                self.ensure_catalog_loaded(&project, &mut cfg);
                if let Some(req_ctx) = v.get("context").and_then(Value::as_str)
                    && !req_ctx.is_empty()
                    && req_ctx != cfg.context
                {
                    return Ok(());
                }
                if let Some(ref mut snap) = self.snapshot {
                    if snap.namespaces.len() <= 3 {
                        fetch_namespaces_into_snapshot(&project, &cfg, snap);
                    }
                } else {
                    self.refresh_cluster(&project);
                }
                self.post_snapshot(false);
            }
            "load-category" | "poll-category" => {
                let is_poll = op == "poll-category";
                let started = std::time::Instant::now();
                let include_metrics = v
                    .get("includeMetrics")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let project = resolve_project(ctx)?;
                let mut cfg = KubeConfigState::load();
                self.ensure_catalog_loaded(&project, &mut cfg);
                if let Some(cat) = v.get("category").and_then(Value::as_str) {
                    if cat == "Workspace" {
                        self.watches.stop_category();
                        self.index_mut(Some(&project), None)
                            .ensure(Some(&project), None);
                        self.post_state(ctx);
                        return Ok(());
                    }
                    if cat == "ClusterCatalog" {
                        self.watches.stop_category();
                        return Ok(());
                    }
                    if !is_poll {
                        self.watches.stop_category();
                    }
                    if let Some(ref mut snap) = self.snapshot {
                        if snap.namespaces.len() <= 3 && !is_poll {
                            fetch_namespaces_into_snapshot(&project, &cfg, snap);
                        }
                        let existing_pi = snap.page_info.get(cat).cloned();
                        // When the user appended multiple pages ("Load more"), skip background poll replacement
                        // so accumulated pages are not wiped out every 5s.
                        if is_poll
                            && let Some(ref pi) = existing_pi
                            && pi.mode == "append"
                            && pi.loaded_count > u64::from(pi.limit)
                        {
                            let elapsed_ms = started.elapsed().as_millis() as u64;
                            self.post_snapshot_meta(false, Some(elapsed_ms));
                            return Ok(());
                        }
                        let limit = v
                            .get("limit")
                            .and_then(Value::as_u64)
                            .map(|n| (n as u32).clamp(10, 500))
                            .or_else(|| existing_pi.as_ref().map(|pi| pi.limit))
                            .unwrap_or(kubectl::DEFAULT_PAGE_LIMIT);
                        let continue_token = v
                            .get("continueToken")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                            .unwrap_or_else(|| {
                                if is_poll {
                                    existing_pi
                                        .as_ref()
                                        .map(|pi| pi.request_token.clone())
                                        .unwrap_or_default()
                                } else {
                                    String::new()
                                }
                            });
                        let append =
                            !is_poll && v.get("pageMode").and_then(Value::as_str) == Some("append");
                        let page_index = v
                            .get("pageIndex")
                            .and_then(Value::as_u64)
                            .map(|n| n.max(1) as u32)
                            .unwrap_or_else(|| {
                                if is_poll {
                                    existing_pi
                                        .as_ref()
                                        .map(|pi| pi.page_index.max(1))
                                        .unwrap_or(1)
                                } else {
                                    1
                                }
                            });
                        fetch_category_with_options(
                            &project,
                            &cfg,
                            snap,
                            cat,
                            FetchOptions {
                                is_background_poll: is_poll,
                                include_metrics,
                                limit,
                                continue_token,
                                append,
                                page_index,
                            },
                        );
                    }
                    self.sync_category_watch(&project, &cfg, cat);
                }
                let elapsed_ms = started.elapsed().as_millis() as u64;
                self.post_snapshot_meta(false, is_poll.then_some(elapsed_ms));
                let allow_drawer_refresh = !is_poll
                    || (self.watches.drawer_stream.is_none()
                        && started.elapsed() < std::time::Duration::from_millis(1200));
                if allow_drawer_refresh
                    && let Some("Pod") = v.get("inspectKind").and_then(Value::as_str)
                    && let Some(ins_name) = v
                        .get("inspectName")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                {
                    let ins_ns = v
                        .get("inspectNs")
                        .and_then(Value::as_str)
                        .unwrap_or("default");
                    if let Ok(out) =
                        inspect_cluster_resource_json(&project, &cfg, "Pod", ins_ns, ins_name)
                    {
                        let msg = json!({
                            "op": "resource-json",
                            "kind": "Pod",
                            "namespace": ins_ns,
                            "name": ins_name,
                            "json": out,
                        });
                        let _ = host::post_view_message(VIEW_ID, &msg.to_string());
                    }
                }
            }
            "poll-watch" => {
                let project = resolve_project(ctx)?;
                let mut cfg = KubeConfigState::load();
                self.ensure_catalog_loaded(&project, &mut cfg);
                let cat = v.get("category").and_then(Value::as_str).unwrap_or("Pod");
                let refresh_metrics = v
                    .get("refreshMetrics")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let drawer_target = match (
                    v.get("inspectKind")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty()),
                    v.get("inspectName")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty()),
                ) {
                    (Some(dk), Some(dname)) => {
                        let dns = v.get("inspectNs").and_then(Value::as_str).unwrap_or("");
                        Some((dk, dns, dname))
                    }
                    _ => None,
                };
                self.poll_active_watches(&project, &cfg, cat, drawer_target, refresh_metrics);
            }
            "stop-drawer-watch" => {
                self.watches.stop_drawer();
            }
            "search-category" => {
                let project = resolve_project(ctx)?;
                let mut cfg = KubeConfigState::load();
                self.ensure_catalog_loaded(&project, &mut cfg);
                let cat = v.get("category").and_then(Value::as_str).unwrap_or("Pod");
                let query = v.get("query").and_then(Value::as_str).unwrap_or("");
                let request_id = v.get("requestId").and_then(Value::as_u64).unwrap_or(0);
                // If the user switched context or namespace while this search message was queued,
                // drop it immediately in 0 ms so subsequent UI actions never queue behind stale scans.
                if let Some(req_ctx) = v
                    .get("context")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    && !cfg.context.trim().is_empty()
                    && req_ctx != cfg.context.trim()
                {
                    return Ok(());
                }
                if let Some(req_sig) = v
                    .get("scopeSignature")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    let cur_sig = if cfg.all_namespaces {
                        "*".to_owned()
                    } else {
                        let mut active = cfg.active_namespaces();
                        active.sort();
                        if active.is_empty() {
                            "*".to_owned()
                        } else {
                            active.join(",")
                        }
                    };
                    if req_sig != cur_sig {
                        return Ok(());
                    }
                }
                let is_cancelled = || _task.as_ref().is_some_and(|t| t.is_cancelled());
                let outcome = if let Some(ref mut snap) = self.snapshot {
                    kubectl::search_cluster_category_ground_truth(
                        &project,
                        &mut self.watches,
                        &cfg,
                        snap,
                        cat,
                        query,
                        &is_cancelled,
                    )
                } else {
                    let mut snap = ClusterSnapshot::default();
                    let out = kubectl::search_cluster_category_ground_truth(
                        &project,
                        &mut self.watches,
                        &cfg,
                        &mut snap,
                        cat,
                        query,
                        &is_cancelled,
                    );
                    self.snapshot = Some(snap);
                    out
                };
                // Tear down the `kubectl proxy` child immediately after the search finishes so
                // it never lingers idle or holds a stale pre-PIM OIDC token in process memory.
                self.watches.stop_proxy();
                Self::send_view_payload(json!({
                    "op": "search-results",
                    "requestId": request_id,
                    "category": outcome.category,
                    "query": outcome.query,
                    "resources": outcome.resources,
                    "events": outcome.events,
                    "totalMatches": outcome.total_matches,
                    "scannedTotal": outcome.scanned_total,
                    "truncated": outcome.truncated,
                    "error": outcome.error,
                }));
            }
            "set-namespace" | "set-namespaces" => {
                self.watches.stop_watches();
                let requested: Vec<String> = if op == "set-namespaces" {
                    v.get("namespaces")
                        .and_then(Value::as_array)
                        .map(|arr| {
                            arr.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                } else if let Some(ns) = v.get("namespace").and_then(Value::as_str) {
                    vec![ns.to_owned()]
                } else {
                    return Ok(());
                };
                let normalized = kubectl::normalize_namespace_list(&requested);
                let mut cfg = KubeConfigState::load();
                cfg.set_namespaces(&normalized);
                cfg.persist_scope();
                let req_limit = v
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map(|n| (n as u32).clamp(10, 500))
                    .unwrap_or(kubectl::DEFAULT_PAGE_LIMIT);
                if let Some(cat) = v.get("category").and_then(Value::as_str)
                    && let Ok(project) = resolve_project(ctx)
                {
                    let _bg_task = (cfg.active_namespaces().len() > 3)
                        .then(|| host::begin_task("Kubernetes: Loading namespaces", false));
                    self.ensure_catalog_loaded(&project, &mut cfg);
                    if let Some(ref mut snap) = self.snapshot {
                        if snap.namespaces.len() <= 3 {
                            fetch_namespaces_into_snapshot(&project, &cfg, snap);
                        }
                        if cfg.all_namespaces
                            && matches!(cat, "ClusterOverview" | "WorkloadsOverview")
                            && enforce_large_cluster_scope(&mut cfg, snap)
                        {
                            snap.error = Some(format!(
                                "Cluster has {} namespaces; Overview is scoped to '{}' for responsiveness (select an individual resource tab to query All Namespaces).",
                                snap.namespaces.len(),
                                cfg.namespace
                            ));
                        }
                        snap.current_namespace = cfg.namespace.clone();
                        snap.all_namespaces = cfg.all_namespaces;
                        snap.selected_namespaces = cfg.active_namespaces();
                        snap.helm_releases.clear();
                        snap.events.clear();
                        snap.page_info.clear();
                        snap.overview_counts.retain(|k, _| k == "Namespace");
                        snap.resources
                            .retain(|r| r.kind == "CustomResourceDefinition");
                        fetch_category_with_options(
                            &project,
                            &cfg,
                            snap,
                            cat,
                            FetchOptions {
                                limit: req_limit,
                                ..FetchOptions::default()
                            },
                        );
                    }
                    self.sync_category_watch(&project, &cfg, cat);
                    self.post_snapshot(false);
                } else {
                    if let Ok(project) = resolve_project(ctx) {
                        self.refresh_cluster(&project);
                    }
                    self.post_state(ctx);
                }
            }
            "set-all-namespaces" => {
                self.watches.stop_watches();
                let all = v.get("all").and_then(Value::as_bool).unwrap_or(false);
                let mut cfg = KubeConfigState::load();
                cfg.all_namespaces = all;
                cfg.persist_scope();
                let req_limit = v
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map(|n| (n as u32).clamp(10, 500))
                    .unwrap_or(kubectl::DEFAULT_PAGE_LIMIT);
                if let Some(cat) = v.get("category").and_then(Value::as_str)
                    && let Ok(project) = resolve_project(ctx)
                {
                    self.ensure_catalog_loaded(&project, &mut cfg);
                    if let Some(ref mut snap) = self.snapshot {
                        if snap.namespaces.len() <= 3 {
                            fetch_namespaces_into_snapshot(&project, &cfg, snap);
                        }
                        if all
                            && matches!(cat, "ClusterOverview" | "WorkloadsOverview")
                            && enforce_large_cluster_scope(&mut cfg, snap)
                        {
                            snap.error = Some(format!(
                                "Cluster has {} namespaces; Overview is scoped to '{}' for responsiveness (select an individual resource tab to query All Namespaces).",
                                snap.namespaces.len(),
                                cfg.namespace
                            ));
                        } else {
                            snap.all_namespaces = all;
                        }
                        snap.selected_namespaces = cfg.active_namespaces();
                        snap.helm_releases.clear();
                        snap.events.clear();
                        snap.page_info.clear();
                        snap.overview_counts.retain(|k, _| k == "Namespace");
                        snap.resources
                            .retain(|r| r.kind == "CustomResourceDefinition");
                        fetch_category_with_options(
                            &project,
                            &cfg,
                            snap,
                            cat,
                            FetchOptions {
                                limit: req_limit,
                                ..FetchOptions::default()
                            },
                        );
                    }
                    self.sync_category_watch(&project, &cfg, cat);
                    self.post_snapshot(false);
                } else {
                    if let Ok(project) = resolve_project(ctx) {
                        self.refresh_cluster(&project);
                    }
                    self.post_state(ctx);
                }
            }
            "inspect" => {
                let project = resolve_project(ctx)?;
                let cfg = KubeConfigState::load();
                let kind = v.get("kind").and_then(Value::as_str).unwrap_or("Pod");
                let ns = v.get("namespace").and_then(Value::as_str).unwrap_or("");
                let name = v.get("name").and_then(Value::as_str).unwrap_or("");
                let as_yaml = v.get("yaml").and_then(Value::as_bool).unwrap_or(false);
                match inspect_cluster_resource(&project, &cfg, kind, ns, name, as_yaml) {
                    Ok(out) => Self::post_detail(out, None, false),
                    Err(e) => Self::post_detail(e.to_string(), Some(e.to_string()), true),
                }
            }
            "inspect-json" => {
                let project = resolve_project(ctx)?;
                let mut cfg = KubeConfigState::load();
                self.ensure_catalog_loaded(&project, &mut cfg);
                let kind = v.get("kind").and_then(Value::as_str).unwrap_or("Pod");
                let ns = v.get("namespace").and_then(Value::as_str).unwrap_or("");
                let name = v.get("name").and_then(Value::as_str).unwrap_or("");
                match inspect_cluster_resource_json(&project, &cfg, kind, ns, name) {
                    Ok(out) => {
                        let rv = extract_object_resource_version(&out);
                        self.sync_drawer_watch(&project, &cfg, kind, ns, name, rv);
                        let msg = json!({
                            "op": "resource-json",
                            "kind": kind,
                            "namespace": ns,
                            "name": name,
                            "json": out,
                            "drawerWatching": self.watches.drawer_stream.is_some(),
                        });
                        let _ = host::post_view_message(VIEW_ID, &msg.to_string());
                        let extras = fetch_resource_extras(&project, &cfg, kind, ns, name, &out);
                        let _ = host::post_view_message(VIEW_ID, &extras.to_string());
                    }
                    Err(e) => {
                        self.watches.stop_drawer();
                        Self::post_detail(e.to_string(), Some(e.to_string()), true);
                    }
                }
            }
            "pod-logs" => {
                let project = resolve_project(ctx)?;
                let cfg = KubeConfigState::load();
                let ns = v
                    .get("namespace")
                    .and_then(Value::as_str)
                    .unwrap_or("default");
                let name = v.get("name").and_then(Value::as_str).unwrap_or("");
                let container = v.get("container").and_then(Value::as_str);
                let previous = v.get("previous").and_then(Value::as_bool).unwrap_or(false);
                match fetch_pod_logs(&project, &cfg, ns, name, container, previous) {
                    Ok(logs) => Self::post_detail(logs, None, false),
                    Err(e) => Self::post_detail(e.to_string(), Some(e.to_string()), true),
                }
            }
            "follow-logs" => {
                let cfg = KubeConfigState::load();
                let ns = v
                    .get("namespace")
                    .and_then(Value::as_str)
                    .unwrap_or("default");
                let name = v.get("name").and_then(Value::as_str).unwrap_or("");
                let container = v.get("container").and_then(Value::as_str);
                open_follow_logs_terminal(&cfg, ns, name, container)?;
            }
            "exec-shell" => {
                let mut cfg = KubeConfigState::load();
                if let Some(c) = v
                    .get("context")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    cfg.context = c.to_owned();
                }
                let ns = v
                    .get("namespace")
                    .and_then(Value::as_str)
                    .unwrap_or("default");
                let name = v.get("name").and_then(Value::as_str).unwrap_or("");
                let container = v.get("container").and_then(Value::as_str);
                let project = resolve_project(ctx).ok();
                if let Err(e) =
                    open_exec_or_debug_terminal(project.as_ref(), &cfg, ns, name, container)
                {
                    let msg = e.to_string();
                    host::notify(NotificationLevel::Error, "Kubernetes Pod Shell", &msg);
                    Self::post_detail(msg.clone(), Some(msg), true);
                }
            }
            "debug-shell" => {
                let mut cfg = KubeConfigState::load();
                if let Some(c) = v
                    .get("context")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    cfg.context = c.to_owned();
                }
                let ns = v
                    .get("namespace")
                    .and_then(Value::as_str)
                    .unwrap_or("default");
                let name = v.get("name").and_then(Value::as_str).unwrap_or("");
                let container = v.get("container").and_then(Value::as_str);
                let image = v.get("image").and_then(Value::as_str);
                let project = resolve_project(ctx).ok();
                open_debug_terminal(project.as_ref(), &cfg, ns, name, container, image)?;
                if let Some(ref proj) = project {
                    if let Some(ref mut snap) = self.snapshot {
                        fetch_category_into_snapshot(proj, &cfg, snap, "Pod");
                    }
                    self.post_snapshot(false);
                    if let Ok(out) = inspect_cluster_resource_json(proj, &cfg, "Pod", ns, name) {
                        let msg = json!({
                            "op": "resource-json",
                            "kind": "Pod",
                            "namespace": ns,
                            "name": name,
                            "json": out,
                        });
                        let _ = host::post_view_message(VIEW_ID, &msg.to_string());
                    }
                }
            }
            "node-shell" => {
                let mut cfg = KubeConfigState::load();
                if let Some(c) = v
                    .get("context")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    cfg.context = c.to_owned();
                }
                let name = v.get("name").and_then(Value::as_str).unwrap_or("");
                let mode = v.get("mode").and_then(Value::as_str);
                let project = resolve_project(ctx).ok();
                open_node_shell_terminal(project.as_ref(), &cfg, name, mode)?;
            }
            "pod-exec" => {
                let project = resolve_project(ctx)?;
                let cfg = KubeConfigState::load();
                let ns = v
                    .get("namespace")
                    .and_then(Value::as_str)
                    .unwrap_or("default");
                let name = v.get("name").and_then(Value::as_str).unwrap_or("");
                let container = v.get("container").and_then(Value::as_str);
                let cmd = v.get("command").and_then(Value::as_str).unwrap_or("pwd");
                let cwd = v.get("cwd").and_then(Value::as_str).unwrap_or("/");
                let out = exec_pod_command(&project, &cfg, ns, name, container, cmd, cwd);
                let _ = host::post_view_message(VIEW_ID, &out.to_string());
            }
            "port-forward" => {
                let mut cfg = KubeConfigState::load();
                if let Some(c) = v
                    .get("context")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    cfg.context = c.to_owned();
                }
                let kind = v.get("kind").and_then(Value::as_str).unwrap_or("Pod");
                let ns = v
                    .get("namespace")
                    .and_then(Value::as_str)
                    .unwrap_or("default");
                let name = v.get("name").and_then(Value::as_str).unwrap_or("");
                let port = v.get("port").and_then(Value::as_str).unwrap_or("8080");
                let local_port = v.get("localPort").and_then(Value::as_str);
                let address = v.get("address").and_then(Value::as_str);
                let https = v.get("https").and_then(Value::as_bool).unwrap_or(false);
                let open_in_browser = v
                    .get("openInBrowser")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                open_port_forward_terminal(
                    &cfg,
                    kind,
                    ns,
                    name,
                    port,
                    local_port,
                    address,
                    https,
                    open_in_browser,
                )?;
            }
            "scale" => {
                let project = resolve_project(ctx)?;
                let cfg = KubeConfigState::load();
                let kind = v
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("Deployment");
                let ns = v
                    .get("namespace")
                    .and_then(Value::as_str)
                    .unwrap_or("default");
                let name = v.get("name").and_then(Value::as_str).unwrap_or("");
                let replicas = v.get("replicas").and_then(Value::as_u64).unwrap_or(1) as u32;
                match scale_workload(&project, &cfg, kind, ns, name, replicas) {
                    Ok(msg) => {
                        self.refresh_cluster(&project);
                        self.post_state(ctx);
                        Self::post_detail(msg.clone(), Some(msg), false);
                    }
                    Err(e) => Self::post_detail(e.to_string(), Some(e.to_string()), true),
                }
            }
            "restart" => {
                let project = resolve_project(ctx)?;
                let cfg = KubeConfigState::load();
                let kind = v
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("Deployment");
                let ns = v
                    .get("namespace")
                    .and_then(Value::as_str)
                    .unwrap_or("default");
                let name = v.get("name").and_then(Value::as_str).unwrap_or("");
                match rollout_restart(&project, &cfg, kind, ns, name) {
                    Ok(msg) => {
                        self.refresh_cluster(&project);
                        self.post_state(ctx);
                        Self::post_detail(msg.clone(), Some(msg), false);
                    }
                    Err(e) => Self::post_detail(e.to_string(), Some(e.to_string()), true),
                }
            }
            "copy-yaml" => {
                let project = resolve_project(ctx)?;
                let cfg = KubeConfigState::load();
                let kind = v.get("kind").and_then(Value::as_str).unwrap_or("Pod");
                let ns = v.get("namespace").and_then(Value::as_str).unwrap_or("");
                let name = v.get("name").and_then(Value::as_str).unwrap_or("");
                let yaml = inspect_cluster_resource(&project, &cfg, kind, ns, name, true)?;
                host::copy_to_clipboard(&yaml)?;
                Self::post_detail(
                    yaml,
                    Some(format!("Copied {kind}/{name} YAML to clipboard")),
                    false,
                );
            }
            "apply-yaml" | "diff-yaml" => {
                let project = resolve_project(ctx)?;
                let cfg = KubeConfigState::load();
                let yaml_text = v.get("yaml").and_then(Value::as_str).unwrap_or("");
                let mode = if op == "apply-yaml" { "apply" } else { "diff" };
                match apply_or_diff_manifest(&project, &cfg, yaml_text, mode) {
                    Ok(out) => {
                        if mode == "apply" {
                            self.refresh_cluster(&project);
                            self.post_state(ctx);
                            host::notify(
                                NotificationLevel::Info,
                                "Kubernetes Apply",
                                out.lines().next().unwrap_or("Applied"),
                            );
                            Self::post_detail(
                                yaml_text,
                                Some(format!("Applied to cluster: {out}")),
                                false,
                            );
                        } else {
                            Self::post_detail(out, Some("Diff against live cluster".into()), false);
                        }
                    }
                    Err(e) => Self::post_detail(yaml_text, Some(e.to_string()), true),
                }
            }
            "delete" => {
                let project = resolve_project(ctx)?;
                let mut cfg = KubeConfigState::load();
                self.ensure_catalog_loaded(&project, &mut cfg);
                let kind = v.get("kind").and_then(Value::as_str).unwrap_or("Pod");
                let ns = v.get("namespace").and_then(Value::as_str).unwrap_or("");
                let name = v.get("name").and_then(Value::as_str).unwrap_or("");
                let force = v.get("force").and_then(Value::as_bool).unwrap_or(false);
                match delete_cluster_resource_opts(&project, &cfg, kind, ns, name, force) {
                    Ok(msg) => {
                        if let Some(ref mut snap) = self.snapshot {
                            snap.resources.retain(|r| {
                                !(r.kind == kind && r.namespace == ns && r.name == name)
                            });
                            if kind == "HelmRelease" {
                                snap.helm_releases
                                    .retain(|r| !(r.namespace == ns && r.name == name));
                            }
                            fetch_category_into_snapshot(&project, &cfg, snap, kind);
                        }
                        self.post_snapshot(false);
                        Self::post_detail(msg.clone(), Some(msg), false);
                    }
                    Err(e) => Self::post_detail(e.to_string(), Some(e.to_string()), true),
                }
            }
            "delete-batch" => {
                let project = resolve_project(ctx)?;
                let mut cfg = KubeConfigState::load();
                self.ensure_catalog_loaded(&project, &mut cfg);
                let force = v.get("force").and_then(Value::as_bool).unwrap_or(false);
                let items = v
                    .get("items")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if items.is_empty() {
                    return Ok(());
                }
                let mut deleted = Vec::new();
                let mut deleted_keys = Vec::new();
                let mut errors = Vec::new();
                let mut touched_kinds = Vec::new();
                for item in &items {
                    let kind = item.get("kind").and_then(Value::as_str).unwrap_or("Pod");
                    let ns = item.get("namespace").and_then(Value::as_str).unwrap_or("");
                    let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                    let item_force = item.get("force").and_then(Value::as_bool).unwrap_or(force);
                    if name.is_empty() {
                        continue;
                    }
                    if !touched_kinds.iter().any(|k: &String| k == kind) {
                        touched_kinds.push(kind.to_owned());
                    }
                    match delete_cluster_resource_opts(&project, &cfg, kind, ns, name, item_force) {
                        Ok(_) => {
                            deleted.push(if ns.is_empty() {
                                format!("{kind}/{name}")
                            } else {
                                format!("{kind}/{ns}/{name}")
                            });
                            deleted_keys.push((kind.to_owned(), ns.to_owned(), name.to_owned()));
                        }
                        Err(e) => errors.push(format!("{kind}/{name}: {e}")),
                    }
                }
                if let Some(ref mut snap) = self.snapshot {
                    for (k, ns, n) in &deleted_keys {
                        snap.resources
                            .retain(|r| !(r.kind == *k && r.namespace == *ns && r.name == *n));
                        if k == "HelmRelease" {
                            snap.helm_releases
                                .retain(|r| !(r.namespace == *ns && r.name == *n));
                        }
                    }
                    for kind in &touched_kinds {
                        fetch_category_into_snapshot(&project, &cfg, snap, kind);
                    }
                }
                self.post_snapshot(false);
                if errors.is_empty() {
                    let msg = format!("Removed {} item(s): {}", deleted.len(), deleted.join(", "));
                    Self::post_detail(msg.clone(), Some(msg), false);
                } else {
                    let msg = format!(
                        "Removed {} item(s); {} failed: {}",
                        deleted.len(),
                        errors.len(),
                        errors.join("; ")
                    );
                    Self::post_detail(msg.clone(), Some(msg), true);
                }
            }
            "cordon-node" => {
                let project = resolve_project(ctx)?;
                let mut cfg = KubeConfigState::load();
                self.ensure_catalog_loaded(&project, &mut cfg);
                let name = v.get("name").and_then(Value::as_str).unwrap_or("");
                let uncordon = v.get("uncordon").and_then(Value::as_bool).unwrap_or(false);
                match cordon_node(&project, &cfg, name, uncordon) {
                    Ok(msg) => {
                        if let Some(ref mut snap) = self.snapshot {
                            fetch_nodes_into_snapshot(&project, &cfg, snap);
                        }
                        self.post_snapshot(false);
                        if let Ok(json_str) =
                            inspect_cluster_resource_json(&project, &cfg, "Node", "", name)
                        {
                            let jmsg = json!({
                                "op": "resource-json",
                                "kind": "Node",
                                "namespace": "",
                                "name": name,
                                "json": json_str,
                            });
                            let _ = host::post_view_message(VIEW_ID, &jmsg.to_string());
                        }
                        host::notify(NotificationLevel::Info, "Kubernetes Node", &msg);
                        Self::post_detail(msg.clone(), Some(msg), false);
                    }
                    Err(e) => Self::post_detail(e.to_string(), Some(e.to_string()), true),
                }
            }
            "drain-node" => {
                let project = resolve_project(ctx)?;
                let mut cfg = KubeConfigState::load();
                self.ensure_catalog_loaded(&project, &mut cfg);
                let name = v.get("name").and_then(Value::as_str).unwrap_or("");
                match drain_node(&project, &cfg, name) {
                    Ok(msg) => {
                        if let Some(ref mut snap) = self.snapshot {
                            fetch_nodes_into_snapshot(&project, &cfg, snap);
                        }
                        self.post_snapshot(false);
                        if let Ok(json_str) =
                            inspect_cluster_resource_json(&project, &cfg, "Node", "", name)
                        {
                            let jmsg = json!({
                                "op": "resource-json",
                                "kind": "Node",
                                "namespace": "",
                                "name": name,
                                "json": json_str,
                            });
                            let _ = host::post_view_message(VIEW_ID, &jmsg.to_string());
                        }
                        host::notify(NotificationLevel::Info, "Kubernetes Node Drain", &msg);
                        Self::post_detail(msg.clone(), Some(msg), false);
                    }
                    Err(e) => Self::post_detail(e.to_string(), Some(e.to_string()), true),
                }
            }
            "trigger-cronjob" => {
                let project = resolve_project(ctx)?;
                let mut cfg = KubeConfigState::load();
                self.ensure_catalog_loaded(&project, &mut cfg);
                let ns = v
                    .get("namespace")
                    .and_then(Value::as_str)
                    .unwrap_or("default");
                let name = v.get("name").and_then(Value::as_str).unwrap_or("");
                match trigger_cronjob(&project, &cfg, ns, name) {
                    Ok(msg) => {
                        if let Some(ref mut snap) = self.snapshot {
                            fetch_category_into_snapshot(&project, &cfg, snap, "CronJob");
                            fetch_category_into_snapshot(&project, &cfg, snap, "Job");
                        }
                        self.post_snapshot(false);
                        if let Ok(json_str) =
                            inspect_cluster_resource_json(&project, &cfg, "CronJob", ns, name)
                        {
                            let extras = fetch_resource_extras(
                                &project, &cfg, "CronJob", ns, name, &json_str,
                            );
                            let _ = host::post_view_message(VIEW_ID, &extras.to_string());
                        }
                        host::notify(NotificationLevel::Info, "Kubernetes CronJob", &msg);
                        Self::post_detail(msg.clone(), Some(msg), false);
                    }
                    Err(e) => Self::post_detail(e.to_string(), Some(e.to_string()), true),
                }
            }
            "suspend-cronjob" => {
                let project = resolve_project(ctx)?;
                let mut cfg = KubeConfigState::load();
                self.ensure_catalog_loaded(&project, &mut cfg);
                let ns = v
                    .get("namespace")
                    .and_then(Value::as_str)
                    .unwrap_or("default");
                let name = v.get("name").and_then(Value::as_str).unwrap_or("");
                let suspend = v.get("suspend").and_then(Value::as_bool).unwrap_or(true);
                match suspend_cronjob(&project, &cfg, ns, name, suspend) {
                    Ok(msg) => {
                        if let Some(ref mut snap) = self.snapshot {
                            fetch_category_into_snapshot(&project, &cfg, snap, "CronJob");
                        }
                        self.post_snapshot(false);
                        if let Ok(json_str) =
                            inspect_cluster_resource_json(&project, &cfg, "CronJob", ns, name)
                        {
                            let jmsg = json!({
                                "op": "resource-json",
                                "kind": "CronJob",
                                "namespace": ns,
                                "name": name,
                                "json": json_str,
                            });
                            let _ = host::post_view_message(VIEW_ID, &jmsg.to_string());
                        }
                        host::notify(NotificationLevel::Info, "Kubernetes CronJob", &msg);
                        Self::post_detail(msg.clone(), Some(msg), false);
                    }
                    Err(e) => Self::post_detail(e.to_string(), Some(e.to_string()), true),
                }
            }
            "helm-detail" => {
                let project = resolve_project(ctx)?;
                let cfg = KubeConfigState::load();
                let ns = v
                    .get("namespace")
                    .and_then(Value::as_str)
                    .unwrap_or("default");
                let name = v.get("name").and_then(Value::as_str).unwrap_or("");
                let sub = v.get("sub").and_then(Value::as_str).unwrap_or("values");
                match fetch_helm_release_detail(&project, &cfg, ns, name, sub) {
                    Ok(out) => Self::post_detail(out, None, false),
                    Err(e) => Self::post_detail(e.to_string(), Some(e.to_string()), true),
                }
            }
            "open-location" => {
                if let Some(path) = v.get("path").and_then(Value::as_str) {
                    let line = v.get("line").and_then(Value::as_u64).unwrap_or(0) as u32;
                    host::open_location(path, line, 0)?;
                }
            }
            "editor-action" => {
                let act = v.get("action").and_then(Value::as_str).unwrap_or("apply");
                match self.run_manifest_command(act, ctx) {
                    Ok(out) => {
                        if act == "apply"
                            && let Ok(project) = resolve_project(ctx)
                        {
                            self.refresh_cluster(&project);
                            self.post_state(ctx);
                        }
                        Self::post_detail(
                            out.clone(),
                            Some(format!("kubectl {act} succeeded")),
                            false,
                        );
                    }
                    Err(e) => Self::post_detail(e.to_string(), Some(e.to_string()), true),
                }
            }
            "kustomize-build" => match self.run_kustomize_command(ctx, None) {
                Ok(out) => Self::post_detail(out, Some("Rendered Kustomize overlay".into()), false),
                Err(e) => Self::post_detail(e.to_string(), Some(e.to_string()), true),
            },
            "helm-template" => match self.run_helm_template_command(ctx, None) {
                Ok(out) => {
                    Self::post_detail(out, Some("Rendered Helm chart templates".into()), false)
                }
                Err(e) => Self::post_detail(e.to_string(), Some(e.to_string()), true),
            },
            "open-resource-yaml" => {
                let project = resolve_project(ctx)?;
                let mut cfg = KubeConfigState::load();
                let req_ctx = v.get("context").and_then(Value::as_str).unwrap_or("");
                if !req_ctx.is_empty() {
                    cfg.context = req_ctx.to_owned();
                }
                let kind = v.get("kind").and_then(Value::as_str).unwrap_or("Pod");
                let ns = v.get("namespace").and_then(Value::as_str).unwrap_or("");
                let name = v.get("name").and_then(Value::as_str).unwrap_or("");
                let title = v
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("{kind}: {name}.yaml"));
                let key = v
                    .get("key")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("k8s-yaml:{}|{ns}|{kind}|{name}", cfg.context));
                match inspect_cluster_resource(&project, &cfg, kind, ns, name, true) {
                    Ok(yaml) => {
                        self.content_tabs.insert(
                            key.clone(),
                            ContentTabTarget {
                                context: cfg.context.clone(),
                                namespace: ns.to_owned(),
                                kind: kind.to_owned(),
                                name: name.to_owned(),
                            },
                        );
                        let msg = json!({
                            "op": "resource-yaml-ready",
                            "key": key,
                            "title": title,
                            "fileName": format!("{name}.yaml"),
                            "content": yaml,
                        });
                        let _ = host::post_view_message(VIEW_ID, &msg.to_string());
                    }
                    Err(e) => Self::post_detail(e.to_string(), Some(e.to_string()), true),
                }
            }
            "content-tab-action" => {
                let request_id = v
                    .get("requestId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let key = v.get("key").and_then(Value::as_str).unwrap_or("");
                let action = v.get("action").and_then(Value::as_str).unwrap_or("");
                let content = v.get("content").and_then(Value::as_str).unwrap_or("");
                let reply = match (resolve_project(ctx), self.resolve_content_tab_target(key)) {
                    (Ok(project), Some(target)) => {
                        let mut cfg = KubeConfigState::load();
                        if !target.context.is_empty() {
                            cfg.context = target.context.clone();
                        }
                        if !target.namespace.is_empty() {
                            cfg.namespace = target.namespace.clone();
                        }
                        match action {
                            "reload" => match inspect_cluster_resource(
                                &project,
                                &cfg,
                                &target.kind,
                                &target.namespace,
                                &target.name,
                                true,
                            ) {
                                Ok(yaml) => json!({
                                    "op": "content-tab-reply",
                                    "requestId": request_id,
                                    "content": yaml,
                                    "saved": true,
                                    "status": format!("Reloaded {}/{} from cluster", target.kind, target.name),
                                }),
                                Err(e) => json!({
                                    "op": "content-tab-reply",
                                    "requestId": request_id,
                                    "error": e.to_string(),
                                }),
                            },
                            "diff" => match inspect_cluster_resource(
                                &project,
                                &cfg,
                                &target.kind,
                                &target.namespace,
                                &target.name,
                                true,
                            ) {
                                Ok(live_yaml) => {
                                    let identical = live_yaml.trim() == content.trim();
                                    let ctx_label = if cfg.context.is_empty() {
                                        "current"
                                    } else {
                                        cfg.context.as_str()
                                    };
                                    json!({
                                        "op": "content-tab-reply",
                                        "requestId": request_id,
                                        "diff": {
                                            "left": live_yaml,
                                            "right": content,
                                            "leftLabel": format!("Cluster ({ctx_label})"),
                                            "rightLabel": "Edited",
                                            "title": format!("Diff: {}/{}", target.kind, target.name),
                                        },
                                        "status": if identical {
                                            "No differences against live cluster resource".to_string()
                                        } else {
                                            format!("Opened diff for {}/{}", target.kind, target.name)
                                        },
                                    })
                                }
                                Err(e) => json!({
                                    "op": "content-tab-reply",
                                    "requestId": request_id,
                                    "error": e.to_string(),
                                }),
                            },
                            "save" | "apply" => {
                                match apply_or_diff_manifest(&project, &cfg, content, "apply") {
                                    Ok(out) => {
                                        if let Some(ref mut snap) = self.snapshot {
                                            fetch_category_into_snapshot(
                                                &project,
                                                &cfg,
                                                snap,
                                                &target.kind,
                                            );
                                            self.post_snapshot(false);
                                        }
                                        let refreshed = inspect_cluster_resource(
                                            &project,
                                            &cfg,
                                            &target.kind,
                                            &target.namespace,
                                            &target.name,
                                            true,
                                        )
                                        .ok();
                                        let preview =
                                            out.lines().next().unwrap_or("Applied").to_string();
                                        host::notify(
                                            NotificationLevel::Info,
                                            "Kubernetes Apply",
                                            &preview,
                                        );
                                        let mut rep = json!({
                                            "op": "content-tab-reply",
                                            "requestId": request_id,
                                            "saved": true,
                                            "status": format!("Applied {}/{}: {preview}", target.kind, target.name),
                                        });
                                        if let Some(updated_yaml) = refreshed {
                                            rep["content"] = Value::String(updated_yaml);
                                        }
                                        rep
                                    }
                                    Err(e) => json!({
                                        "op": "content-tab-reply",
                                        "requestId": request_id,
                                        "saved": false,
                                        "error": e.to_string(),
                                    }),
                                }
                            }
                            other => json!({
                                "op": "content-tab-reply",
                                "requestId": request_id,
                                "error": format!("Unknown content tab action: {other}"),
                            }),
                        }
                    }
                    (Err(e), _) => json!({
                        "op": "content-tab-reply",
                        "requestId": request_id,
                        "saved": false,
                        "error": e.to_string(),
                    }),
                    (_, None) => json!({
                        "op": "content-tab-reply",
                        "requestId": request_id,
                        "saved": false,
                        "error": format!("Unknown Kubernetes resource tab key: {key}"),
                    }),
                };
                let _ = host::post_view_message(VIEW_ID, &reply.to_string());
            }
            _ => {}
        }
        Ok(())
    }

    fn run_manifest_command(
        &mut self,
        subcommand: &str,
        ctx: &Invocation,
    ) -> Result<String, Error> {
        let editor = ctx.editor().ok_or_else(|| {
            Error::Invalid("focus a Kubernetes YAML file in the editor first".into())
        })?;
        let doc = editor.document()?;
        let project = resolve_project(ctx)?;
        let cfg = KubeConfigState::load();
        let out = apply_or_diff_manifest(&project, &cfg, &doc.text, subcommand)?;
        let title = match subcommand {
            "apply" => "Kubernetes Apply",
            "diff" => "Kubernetes Diff",
            "delete" => "Kubernetes Delete",
            "dry-run" => "Kubernetes Dry Run",
            _ => "Kubernetes",
        };
        let preview = out.lines().next().unwrap_or("Done");
        host::notify(NotificationLevel::Info, title, preview);
        Ok(out)
    }

    fn run_kustomize_command(&self, ctx: &Invocation, arg: Option<&str>) -> Result<String, Error> {
        let project = resolve_project(ctx)?;
        let dir = arg
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                ctx.editor().map(|e| {
                    let p = normalize_slashes(&e.path());
                    let root = project
                        .root()
                        .ok()
                        .map(|r| normalize_slashes(&r))
                        .unwrap_or_default();
                    let rel = p
                        .strip_prefix(&root)
                        .map(|r| r.trim_start_matches('/'))
                        .unwrap_or(&p);
                    match rel.rfind('/') {
                        Some(i) => rel[..i].to_owned(),
                        None => ".".into(),
                    }
                })
            })
            .unwrap_or_else(|| ".".into());
        let out = build_kustomize(&project, &dir)?;
        let lines = out.lines().count();
        host::notify(
            NotificationLevel::Info,
            "Kustomize Build",
            &format!("Rendered {lines} lines from '{dir}'"),
        );
        Ok(out)
    }

    fn run_helm_template_command(
        &self,
        ctx: &Invocation,
        arg: Option<&str>,
    ) -> Result<String, Error> {
        let project = resolve_project(ctx)?;
        let dir = arg.filter(|s| !s.is_empty()).unwrap_or(".");
        let out = render_helm_template(&project, dir)?;
        let lines = out.lines().count();
        host::notify(
            NotificationLevel::Info,
            "Helm Template",
            &format!("Rendered {lines} lines from chart '{dir}'"),
        );
        Ok(out)
    }
}

impl KubernetesPlugin {
    fn icon_set_id(root: &str) -> String {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in root.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x100000001b3);
        }
        format!("k8s-icons:{h:016x}")
    }

    /// Publishes dynamic file & folder icons (`kubernetes`, `helm-*`, `kustomize-*`) for `project`.
    pub fn publish_project_icons(&self, project: &Project) {
        let Ok(raw_root) = project.root() else {
            return;
        };
        let norm_root = normalize_slashes(&raw_root);
        let Some(idx) = self.index_for(Some(project), None) else {
            return;
        };
        let items: Vec<(String, String)> = idx
            .compute_workspace_icons(Some(&norm_root))
            .into_iter()
            .map(|(p, icon)| (p, icon.as_str().to_owned()))
            .collect();
        let _ = host::set_file_icons(&Self::icon_set_id(&norm_root), &items);
    }

    /// Scans all open projects and publishes their Kubernetes / Helm / Kustomize file & folder icons.
    pub fn sync_all_project_icons(&mut self) {
        let Ok(projects) = host::projects() else {
            return;
        };
        let mut open_keys = std::collections::BTreeSet::new();
        for proj in projects {
            let key = Self::project_index_key(Some(&proj), None);
            open_keys.insert(key);
            {
                let idx = self.index_mut(Some(&proj), None);
                idx.invalidate();
                idx.ensure(Some(&proj), None);
            }
            self.publish_project_icons(&proj);
        }
        self.indices
            .retain(|k, _| k.starts_with("standalone:") || open_keys.contains(k));
    }
}

impl Plugin for KubernetesPlugin {
    fn activate(&mut self, _ctx: &Context) -> Result<(), Error> {
        let cfg = KubeConfigState::load();
        self.update_status_bar(&cfg);
        let _ = host::subscribe_paths(
            "files-changed",
            &[
                "**/*.yaml",
                "**/*.yml",
                "**/*.tpl",
                "**/Kustomization",
                "**/.helmignore",
                "**/NOTES.txt",
                "**/Chart.lock",
                "**/requirements.lock",
                "**/values.schema.json",
                "**/*.kubeconfig",
                "**/kubeconfig",
                "**/*.json",
            ],
        );
        self.sync_all_project_icons();
        Ok(())
    }

    fn deactivate(&mut self) {
        self.watches.stop_all();
    }

    fn run_command(
        &mut self,
        id: &str,
        ctx: &Invocation,
        arg: Option<&str>,
    ) -> Result<Option<String>, Error> {
        match id {
            "refresh" => {
                let project = resolve_project(ctx)?;
                self.invalidate_project_index(Some(&project));
                let snap = self.refresh_cluster(&project);
                self.post_state(ctx);
                if let Some(err) = snap.error {
                    return Err(Error::Failed(err));
                }
                let msg = format!(
                    "Context '{}' ({}): {} resources loaded",
                    snap.current_context,
                    if snap.all_namespaces {
                        "all namespaces"
                    } else {
                        snap.current_namespace.as_str()
                    },
                    snap.resources.len()
                );
                host::notify(NotificationLevel::Info, "Kubernetes", &msg);
                Ok(Some(msg))
            }
            "switch-context" => {
                let project = resolve_project(ctx)?;
                let cfg = KubeConfigState::load();
                let next = if let Some(explicit) = arg.filter(|s| !s.trim().is_empty()) {
                    explicit.trim().to_owned()
                } else {
                    let (current, contexts, _, _) = fetch_kubeconfig_overview(&project, &cfg)?;
                    if contexts.is_empty() {
                        return Err(Error::NotFound("no contexts found in kubeconfig".into()));
                    }
                    let pos = contexts.iter().position(|c| *c == current).unwrap_or(0);
                    contexts[(pos + 1) % contexts.len()].clone()
                };
                storage::set("context", &next)?;
                let mut updated = KubeConfigState::load();
                updated.context = next.clone();
                self.update_status_bar(&updated);
                self.refresh_cluster(&project);
                self.post_state(ctx);
                host::notify(
                    NotificationLevel::Info,
                    "Kubernetes Context",
                    &format!("Active context set to '{next}'"),
                );
                Ok(Some(next))
            }
            "switch-namespace" => {
                let project = resolve_project(ctx)?;
                let next = if let Some(explicit) = arg.filter(|s| !s.trim().is_empty()) {
                    explicit.trim().to_owned()
                } else {
                    let snap = match self.snapshot.clone() {
                        Some(s) if !s.namespaces.is_empty() => s,
                        _ => self.refresh_cluster(&project),
                    };
                    let cur = &snap.current_namespace;
                    let pos = snap.namespaces.iter().position(|n| n == cur).unwrap_or(0);
                    snap.namespaces
                        .get((pos + 1) % snap.namespaces.len().max(1))
                        .cloned()
                        .unwrap_or_else(|| "default".into())
                };
                let mut updated = KubeConfigState::load();
                updated.set_namespaces([next.as_str()]);
                updated.persist_scope();
                self.update_status_bar(&updated);
                self.refresh_cluster(&project);
                self.post_state(ctx);
                host::notify(
                    NotificationLevel::Info,
                    "Kubernetes Namespace",
                    &format!("Active namespace set to '{next}'"),
                );
                Ok(Some(next))
            }
            "apply" => self.run_manifest_command("apply", ctx).map(Some),
            "diff" => self.run_manifest_command("diff", ctx).map(Some),
            "delete-manifest" => self.run_manifest_command("delete", ctx).map(Some),
            "dry-run" => {
                if let Some(editor) = ctx.editor() {
                    let _ = self.check_and_publish(&editor.path(), true);
                }
                self.run_manifest_command("dry-run", ctx).map(Some)
            }
            "inspect" => {
                let editor = ctx
                    .editor()
                    .ok_or_else(|| Error::Invalid("focus a YAML file first".into()))?;
                let path = editor.path();
                let count = self.check_and_publish(&path, false)?;
                let msg = format!("{count} issue(s) found");
                host::notify(NotificationLevel::Info, "Kubernetes Inspection", &msg);
                Ok(Some(msg))
            }
            "scan-project" => self.scan_project_manifests(ctx).map(Some),
            "follow-logs" => {
                let cfg = KubeConfigState::load();
                let (ns, pod, container) = self.resolve_pod_target(ctx, arg, &cfg)?;
                open_follow_logs_terminal(&cfg, &ns, &pod, container.as_deref())?;
                Ok(Some(format!("{ns}/{pod}")))
            }
            "exec-shell" => {
                let cfg = KubeConfigState::load();
                let (ns, pod, container) = self.resolve_pod_target(ctx, arg, &cfg)?;
                open_exec_terminal(&cfg, &ns, &pod, container.as_deref())?;
                Ok(Some(format!("{ns}/{pod}")))
            }
            "port-forward" => {
                let cfg = KubeConfigState::load();
                let (kind, ns, name, port) = self.resolve_port_forward_target(ctx, arg, &cfg)?;
                open_port_forward_terminal(
                    &cfg, &kind, &ns, &name, &port, None, None, false, false,
                )?;
                Ok(Some(format!("{kind}/{name}:{port}")))
            }
            "scale" => {
                let project = resolve_project(ctx)?;
                let cfg = KubeConfigState::load();
                let (kind, ns, name, replicas) = self.resolve_scale_target(ctx, arg, &cfg)?;
                let out = scale_workload(&project, &cfg, &kind, &ns, &name, replicas)?;
                host::notify(NotificationLevel::Info, "Kubernetes Scale", &out);
                Ok(Some(out))
            }
            "rollout-restart" => {
                let project = resolve_project(ctx)?;
                let cfg = KubeConfigState::load();
                let (kind, ns, name) = self.resolve_workload_target(ctx, arg, &cfg)?;
                let out = rollout_restart(&project, &cfg, &kind, &ns, &name)?;
                host::notify(NotificationLevel::Info, "Kubernetes Rollout Restart", &out);
                Ok(Some(out))
            }
            "kustomize-build" => self.run_kustomize_command(ctx, arg).map(Some),
            "helm-template" => self.run_helm_template_command(ctx, arg).map(Some),
            _ => Err(Error::NotFound(format!("command {id}"))),
        }
    }

    fn handle_events(&mut self, events: &[Event], _ctx: &Invocation) {
        let _task = host::begin_task("Kubernetes: Inspecting manifest", false);
        let validate_on_save = host::setting_bool("validateOnSave").unwrap_or(true);
        let mut needs_full_icon_resync = false;
        for event in events {
            match event {
                Event::FileOpened(path) | Event::FileSaved(path) => {
                    if path.ends_with(".yaml") || path.ends_with(".yml") {
                        if validate_on_save {
                            let _ = self.check_and_publish(path, false);
                        } else if let Some(project) = project_of(path)
                            && let Ok(text) = project.read_file(path)
                        {
                            self.index_mut(Some(&project), Some(path))
                                .ensure(Some(&project), Some((path, &text)));
                            self.publish_project_icons(&project);
                        }
                    } else {
                        needs_full_icon_resync = true;
                    }
                }
                Event::Other { kind, .. } if kind == "files-changed" => {
                    needs_full_icon_resync = true;
                }
                _ => {}
            }
        }
        if needs_full_icon_resync {
            self.sync_all_project_icons();
        }
    }
}

impl KubernetesPlugin {
    /// Resolves `(namespace, pod_name, container)` from `arg` (`ns/pod/container` or `pod`),
    /// active editor YAML, or the first running Pod in the cluster snapshot.
    fn resolve_pod_target(
        &mut self,
        ctx: &Invocation,
        arg: Option<&str>,
        cfg: &KubeConfigState,
    ) -> Result<(String, String, Option<String>), Error> {
        if let Some(a) = arg.filter(|s| !s.trim().is_empty()) {
            let parts: Vec<&str> = a.trim().split('/').collect();
            return Ok(match parts.as_slice() {
                [pod] => (cfg.namespace.clone(), (*pod).to_owned(), None),
                [ns, pod] => ((*ns).to_owned(), (*pod).to_owned(), None),
                [ns, pod, c, ..] => ((*ns).to_owned(), (*pod).to_owned(), Some((*c).to_owned())),
                _ => (cfg.namespace.clone(), a.trim().to_owned(), None),
            });
        }
        if let Some(editor) = ctx.editor()
            && let Ok(doc) = editor.document()
        {
            let parsed = parse_yaml(&doc.text);
            if let Some(kdoc) = parsed
                .docs
                .iter()
                .find(|d| d.kind.as_ref().is_some_and(|k| k.value == "Pod"))
                && let Some(ref n) = kdoc.name
            {
                let ns = kdoc
                    .namespace
                    .as_ref()
                    .map_or_else(|| cfg.namespace.clone(), |s| s.value.clone());
                let c = kdoc
                    .containers
                    .first()
                    .and_then(|c| c.name.as_ref().map(|s| s.value.clone()));
                return Ok((ns, n.value.clone(), c));
            }
        }
        if self.snapshot.is_none()
            && let Ok(project) = resolve_project(ctx)
        {
            self.refresh_cluster(&project);
        }
        if let Some(ref snap) = self.snapshot
            && let Some(pod) = snap.resources.iter().find(|r| r.kind == "Pod")
        {
            let ns = if pod.namespace.is_empty() {
                cfg.namespace.clone()
            } else {
                pod.namespace.clone()
            };
            return Ok((ns, pod.name.clone(), pod.containers.first().cloned()));
        }
        Err(Error::NotFound(
            "no Pod found in active editor or cluster".into(),
        ))
    }

    fn resolve_port_forward_target(
        &mut self,
        ctx: &Invocation,
        arg: Option<&str>,
        cfg: &KubeConfigState,
    ) -> Result<(String, String, String, String), Error> {
        if let Some(a) = arg.filter(|s| !s.trim().is_empty()) {
            // Format: `Service/ns/my-svc:8080:80` or `my-pod:8080`
            let (target, port) = a.trim().split_once(':').unwrap_or((a.trim(), "8080"));
            let parts: Vec<&str> = target.split('/').collect();
            return Ok(match parts.as_slice() {
                [name] => (
                    "Pod".into(),
                    cfg.namespace.clone(),
                    (*name).to_owned(),
                    port.to_owned(),
                ),
                [kind, name] => (
                    (*kind).to_owned(),
                    cfg.namespace.clone(),
                    (*name).to_owned(),
                    port.to_owned(),
                ),
                [kind, ns, name, ..] => (
                    (*kind).to_owned(),
                    (*ns).to_owned(),
                    (*name).to_owned(),
                    port.to_owned(),
                ),
                _ => (
                    "Pod".into(),
                    cfg.namespace.clone(),
                    target.to_owned(),
                    port.to_owned(),
                ),
            });
        }
        if let Some(editor) = ctx.editor()
            && let Ok(doc) = editor.document()
        {
            let parsed = parse_yaml(&doc.text);
            if let Some(kdoc) = parsed.docs.first()
                && let (Some(kind), Some(name)) = (&kdoc.kind, &kdoc.name)
            {
                let ns = kdoc
                    .namespace
                    .as_ref()
                    .map_or_else(|| cfg.namespace.clone(), |s| s.value.clone());
                let port = kdoc
                    .service_ports
                    .first()
                    .and_then(|p| p.port.as_ref().map(|s| s.value.clone()))
                    .or_else(|| {
                        kdoc.containers
                            .first()
                            .and_then(|c| c.ports.first().map(|p| p.value.clone()))
                    })
                    .unwrap_or_else(|| "8080".into());
                return Ok((kind.value.clone(), ns, name.value.clone(), port));
            }
        }
        let (ns, pod, _) = self.resolve_pod_target(ctx, None, cfg)?;
        Ok(("Pod".into(), ns, pod, "8080".into()))
    }

    fn resolve_workload_target(
        &mut self,
        ctx: &Invocation,
        arg: Option<&str>,
        cfg: &KubeConfigState,
    ) -> Result<(String, String, String), Error> {
        if let Some(a) = arg.filter(|s| !s.trim().is_empty()) {
            let parts: Vec<&str> = a.trim().split('/').collect();
            return Ok(match parts.as_slice() {
                [name] => (
                    "Deployment".into(),
                    cfg.namespace.clone(),
                    (*name).to_owned(),
                ),
                [kind, name] => (
                    (*kind).to_owned(),
                    cfg.namespace.clone(),
                    (*name).to_owned(),
                ),
                [kind, ns, name, ..] => ((*kind).to_owned(), (*ns).to_owned(), (*name).to_owned()),
                _ => (
                    "Deployment".into(),
                    cfg.namespace.clone(),
                    a.trim().to_owned(),
                ),
            });
        }
        if let Some(editor) = ctx.editor()
            && let Ok(doc) = editor.document()
        {
            let parsed = parse_yaml(&doc.text);
            if let Some(kdoc) = parsed.docs.iter().find(|d| d.is_workload())
                && let (Some(kind), Some(name)) = (&kdoc.kind, &kdoc.name)
            {
                let ns = kdoc
                    .namespace
                    .as_ref()
                    .map_or_else(|| cfg.namespace.clone(), |s| s.value.clone());
                return Ok((kind.value.clone(), ns, name.value.clone()));
            }
        }
        if self.snapshot.is_none()
            && let Ok(project) = resolve_project(ctx)
        {
            self.refresh_cluster(&project);
        }
        if let Some(ref snap) = self.snapshot
            && let Some(w) = snap
                .resources
                .iter()
                .find(|r| matches!(r.kind.as_str(), "Deployment" | "StatefulSet" | "DaemonSet"))
        {
            return Ok((w.kind.clone(), w.namespace.clone(), w.name.clone()));
        }
        Err(Error::NotFound(
            "no Deployment/StatefulSet/DaemonSet found".into(),
        ))
    }

    fn resolve_scale_target(
        &mut self,
        ctx: &Invocation,
        arg: Option<&str>,
        cfg: &KubeConfigState,
    ) -> Result<(String, String, String, u32), Error> {
        if let Some(a) = arg.filter(|s| !s.trim().is_empty())
            && let Some((target, rep_str)) = a.trim().split_once('=')
        {
            let replicas = rep_str
                .parse::<u32>()
                .map_err(|_| Error::Invalid(format!("invalid replica count: {rep_str}")))?;
            let (kind, ns, name) = self.resolve_workload_target(ctx, Some(target), cfg)?;
            return Ok((kind, ns, name, replicas));
        }
        let (kind, ns, name) = self.resolve_workload_target(ctx, arg, cfg)?;
        let replicas = ctx
            .editor()
            .and_then(|e| e.document().ok())
            .and_then(|d| {
                parse_yaml(&d.text)
                    .docs
                    .into_iter()
                    .find_map(|k| k.replicas.and_then(|r| r.value.parse::<u32>().ok()))
            })
            .unwrap_or(1);
        Ok((kind, ns, name, replicas))
    }
}

impl Providers for KubernetesPlugin {
    fn completion(
        &mut self,
        doc: &Document,
        at: Position,
        _trigger: Option<&str>,
    ) -> Vec<CompletionItem> {
        let proj = project_of(&doc.path);
        let mut items = {
            let idx = self.index_mut(proj.as_ref(), Some(&doc.path));
            idx.ensure(proj.as_ref(), Some((&doc.path, &doc.text)));
            let parsed = idx
                .files
                .get(&normalize_slashes(&doc.path))
                .cloned()
                .unwrap_or_else(|| parse_yaml(&doc.text));
            completions_at(idx, doc, at, &parsed)
        };

        // Enrich ConfigMap/Secret/Service/ServiceAccount/PVC/CRD completions from the cached live cluster snapshot!
        if manifest::helm_action_context_at(&doc.text, at).is_none()
            && let Some(ref snap) = self.snapshot
        {
            let (parent_path, current_line, _) = manifest::yaml_context_at(&doc.text, at);
            let trimmed = current_line.trim_start();
            if let Some(colon) = trimmed.find(':') {
                let key = trimmed[..colon].trim().trim_start_matches("- ").trim();
                let raw_val = trimmed[colon + 1..].trim_start();
                let start_col = at.character.saturating_sub(utf16_len(raw_val));
                let replace = Range::new(Position::new(at.line, start_col), at);
                let full_key = if parent_path.is_empty() {
                    key.to_owned()
                } else {
                    format!("{parent_path}.{key}")
                };

                let target_kind = if full_key.contains("configMapKeyRef.name")
                    || full_key.contains("configMapRef.name")
                    || full_key.ends_with("configMap.name")
                {
                    Some("ConfigMap")
                } else if full_key.contains("secretKeyRef.name")
                    || full_key.contains("secretRef.name")
                    || full_key.ends_with("secret.secretName")
                    || full_key.ends_with("tls.secretName")
                {
                    Some("Secret")
                } else if full_key.ends_with("backend.service.name") {
                    Some("Service")
                } else if full_key.ends_with("persistentVolumeClaim.claimName") {
                    Some("PersistentVolumeClaim")
                } else {
                    None
                };

                if let Some(tk) = target_kind {
                    for r in snap.resources.iter().filter(|r| r.kind == tk) {
                        if !items.iter().any(|i| i.label == r.name) {
                            items.push(CompletionItem {
                                replace: Some(replace),
                                detail: Some(format!("Cluster {tk} ({})", r.namespace)),
                                ..CompletionItem::new(&r.name, CompletionKind::Reference)
                            });
                        }
                    }
                } else if full_key.ends_with("configMapKeyRef.key")
                    || full_key.ends_with("secretKeyRef.key")
                {
                    let tk = if full_key.contains("configMapKeyRef") {
                        "ConfigMap"
                    } else {
                        "Secret"
                    };
                    for r in snap.resources.iter().filter(|r| r.kind == tk) {
                        for dk in &r.data_keys {
                            if !items.iter().any(|i| i.label == *dk) {
                                items.push(CompletionItem {
                                    replace: Some(replace),
                                    detail: Some(format!(
                                        "Cluster {tk} '{}' ({})",
                                        r.name, r.namespace
                                    )),
                                    ..CompletionItem::new(dk, CompletionKind::Property)
                                });
                            }
                        }
                    }
                } else if full_key == "metadata.namespace" {
                    for ns in &snap.namespaces {
                        if !items.iter().any(|i| i.label == *ns) {
                            items.push(CompletionItem {
                                replace: Some(replace),
                                detail: Some("Cluster Namespace".into()),
                                ..CompletionItem::new(ns, CompletionKind::Module)
                            });
                        }
                    }
                }
            }
        }

        items
    }

    fn code_actions(&mut self, doc: &Document, selection: Range) -> Vec<CodeAction> {
        let proj = project_of(&doc.path);
        let warn_latest = host::setting_bool("warnLatestTag").unwrap_or(true);
        let warn_resources = host::setting_bool("warnMissingResources").unwrap_or(true);
        let issues = {
            let idx = self.index_mut(proj.as_ref(), Some(&doc.path));
            idx.ensure(proj.as_ref(), Some((&doc.path, &doc.text)));
            let parsed = idx
                .files
                .get(&normalize_slashes(&doc.path))
                .cloned()
                .unwrap_or_else(|| parse_yaml(&doc.text));
            inspect_document(
                idx,
                &doc.path,
                &doc.text,
                &parsed,
                warn_latest,
                warn_resources,
            )
        };
        let mut actions = Vec::new();
        let mut seen_titles = std::collections::BTreeSet::new();
        for issue in issues {
            let d_line = issue.diagnostic.range.start.line;
            let overlaps = d_line >= selection.start.line && d_line <= selection.end.line;
            let is_file_level = issue
                .fix
                .as_ref()
                .is_some_and(|f| f.title.contains("# nonk8s"));
            if (overlaps || is_file_level)
                && let Some(fix) = issue.fix
                && seen_titles.insert(fix.title.clone())
            {
                actions.push(fix);
            }
        }
        actions
    }

    fn definition(&mut self, doc: &Document, at: Position) -> Vec<Location> {
        let proj = project_of(&doc.path);
        let idx = self.index_mut(proj.as_ref(), Some(&doc.path));
        idx.ensure(proj.as_ref(), Some((&doc.path, &doc.text)));
        let parsed = idx
            .files
            .get(&normalize_slashes(&doc.path))
            .cloned()
            .unwrap_or_else(|| parse_yaml(&doc.text));
        definitions_at(idx, doc, at, &parsed)
    }

    fn line_markers(&mut self, doc: &Document) -> Vec<LineMarker> {
        let proj = project_of(&doc.path);
        let idx = self.index_mut(proj.as_ref(), Some(&doc.path));
        idx.ensure(proj.as_ref(), Some((&doc.path, &doc.text)));
        let parsed = idx
            .files
            .get(&normalize_slashes(&doc.path))
            .cloned()
            .unwrap_or_else(|| parse_yaml(&doc.text));
        line_markers_for(idx, doc, &parsed)
    }
}

impl Views for KubernetesPlugin {
    fn render(
        &mut self,
        view: &str,
        _viewport: Option<Viewport>,
        ctx: &Invocation,
    ) -> Result<View, Error> {
        if view != VIEW_ID {
            return Err(Error::NotFound(view.into()));
        }
        // Only load kubeconfig contexts & workspace manifests synchronously during initial render
        // so the tool window opens immediately and never times out; the page then requests
        // live cluster resources via `postMessage({op: "refresh"})` inside a background Task.
        let project = resolve_project(ctx).ok();
        let root = project
            .as_ref()
            .and_then(|p| p.root().ok())
            .unwrap_or_default();
        self.index_mut(project.as_ref(), None)
            .ensure(project.as_ref(), None);
        if let Some(ref p) = project {
            self.publish_project_icons(p);
        }
        let mut cfg = KubeConfigState::load();
        if let Some(ref p) = project {
            self.ensure_catalog_loaded(p, &mut cfg);
        }
        self.update_status_bar(&cfg);
        let snap = self.snapshot.clone().unwrap_or_default();
        let mut initial_snap = snap.clone();
        if initial_snap.resources.len() > MAX_INITIAL_HTML_RESOURCES {
            initial_snap.resources.truncate(MAX_INITIAL_HTML_RESOURCES);
        }
        let empty = ProjectIndex::default();
        let proj_index = self.index_for(project.as_ref(), None).unwrap_or(&empty);
        let ws = workspace_manifests_json(proj_index, &root);
        let ws_file_count = proj_index.files.len();
        let initial = json!({
            "op": "state",
            "snapshot": initial_snap,
            "workspace": ws,
            "needsClusterFetch": snap.resources.is_empty() && snap.error.is_none(),
        });
        let status_line = if let Some(ref err) = snap.error {
            format!("Disconnected: {err}")
        } else {
            format!(
                "Context: {} • Namespace: {} • {} cluster resources • {} workspace files",
                if snap.current_context.is_empty() {
                    "default"
                } else {
                    snap.current_context.as_str()
                },
                if snap.all_namespaces {
                    "all"
                } else {
                    snap.current_namespace.as_str()
                },
                snap.resources.len(),
                ws_file_count
            )
        };
        let initial_b64 = base64_encode(initial.to_string().as_bytes());
        let html = PAGE_HTML.replacen("__START_B64__", &initial_b64, 1);
        Ok(View::new(Body::Html(html))
            .button(Button::new("refresh", "Refresh Cluster").icon("refresh"))
            .button(Button::new("apply", "Apply Active YAML").icon("run"))
            .button(Button::new("scan", "Inspect Project Manifests"))
            .status(&status_line))
    }

    fn handle(&mut self, view: &str, event: ViewEvent, ctx: &Invocation) -> Result<(), Error> {
        if view != VIEW_ID {
            return Err(Error::NotFound(view.into()));
        }
        match event {
            ViewEvent::Button(id) => match id.as_str() {
                "refresh" => {
                    let project = resolve_project(ctx)?;
                    self.invalidate_project_index(Some(&project));
                    self.refresh_cluster(&project);
                    self.post_state(ctx);
                    host::refresh_view(VIEW_ID);
                }
                "apply" => {
                    let out = self.run_manifest_command("apply", ctx)?;
                    if let Ok(project) = resolve_project(ctx) {
                        self.refresh_cluster(&project);
                    }
                    self.post_state(ctx);
                    Self::post_detail(out, Some("Applied active manifest".into()), false);
                }
                "scan" => {
                    let summary = self.scan_project_manifests(ctx)?;
                    Self::post_detail(summary.clone(), Some(summary), false);
                }
                _ => {}
            },
            ViewEvent::Message(raw) => {
                self.handle_page_message(&raw, ctx)?;
            }
            _ => {}
        }
        Ok(())
    }
}

register_plugin!(KubernetesPlugin, providers, views);

#[cfg(test)]
mod tests {
    use super::*;
    use openidea_plugin::ProcessOutput;
    use openidea_plugin::mock::MockHost;

    const SAMPLE_MANIFEST: &str = r#"apiVersion: v1
kind: ConfigMap
metadata:
  name: app-config
data:
  DATABASE_URL: "postgres://db:5432/app"
  LOG_LEVEL: "info"
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web-api
  labels:
    app.kubernetes.io/name: web-api
spec:
  replicas: 2
  selector:
    matchLabels:
      app.kubernetes.io/name: web-api
  template:
    metadata:
      labels:
        app.kubernetes.io/name: web-api
    spec:
      serviceAccountName: web-sa
      containers:
        - name: api
          image: ghcr.io/acme/web-api:1.4.2
          ports:
            - name: http
              containerPort: 8080
          env:
            - name: DB
              valueFrom:
                configMapKeyRef:
                  name: app-config
                  key: DATABASE_URL
          resources:
            requests:
              cpu: 100m
              memory: 128Mi
            limits:
              cpu: 500m
              memory: 256Mi
---
apiVersion: v1
kind: Service
metadata:
  name: web-svc
spec:
  type: ClusterIP
  selector:
    app.kubernetes.io/name: web-api
  ports:
    - name: http
      port: 80
      targetPort: http
---
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: web-ing
spec:
  rules:
    - host: api.example.com
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: web-svc
                port:
                  name: http
"#;

    const KUBECONFIG_JSON: &str = r#"{
        "current-context": "prod-east",
        "contexts": [
            {"name": "dev-local", "context": {"namespace": "dev"}},
            {"name": "prod-east", "context": {"namespace": "production"}}
        ]
    }"#;

    const RESOURCES_JSON: &str = r#"{
        "kind": "List",
        "items": [
            {
                "kind": "Pod",
                "metadata": {"name": "web-api-7f8b9-abc", "namespace": "production", "creationTimestamp": "2026-11-05T12:00:00Z"},
                "spec": {"nodeName": "node-1", "containers": [{"name": "api", "ports": [{"containerPort": 8080}]}]},
                "status": {
                    "phase": "Running",
                    "podIP": "10.244.0.14",
                    "containerStatuses": [{"name": "api", "ready": true, "restartCount": 1}]
                }
            },
            {
                "kind": "Deployment",
                "metadata": {"name": "web-api", "namespace": "production"},
                "spec": {"replicas": 3, "containers": [{"name": "api"}]},
                "status": {"readyReplicas": 3, "updatedReplicas": 3}
            },
            {
                "kind": "ConfigMap",
                "metadata": {"name": "cluster-cm", "namespace": "production"},
                "data": {"CLUSTER_KEY": "1", "FEATURE_FLAG": "true"}
            }
        ]
    }"#;

    fn install_mock_with_kubectl() -> MockHost {
        let host = MockHost::install().with_capabilities(&[
            "project.read",
            "editor.read",
            "editor.write",
            "events",
            "storage",
            "clipboard",
            "net:127.0.0.1",
            "process:kubectl",
            "process:helm",
            "process:kustomize",
        ]);
        host.on_process("kubectl", |args, _stdin| {
            let joined = args.join(" ");
            let stdout = if joined.contains("config view") {
                KUBECONFIG_JSON.to_owned()
            } else if joined.contains("get namespaces") {
                "default Active 10d\nproduction Active 10d\n".to_owned()
            } else if joined.contains("get nodes") {
                r#"{"kind":"List","items":[{"kind":"Node","metadata":{"name":"node-1"},"status":{"conditions":[{"type":"Ready","status":"True"}],"nodeInfo":{"kubeletVersion":"v1.32.0"}}}]}"#.to_owned()
            } else if joined.contains("get crd") {
                "certificates.cert-manager.io cert-manager.io Certificate Namespaced 10d\n".to_owned()
            } else if joined.contains("get pods,deployments") {
                RESOURCES_JSON.to_owned()
            } else if joined.contains("get secrets") {
                "production app-secret Opaque 5d\n".to_owned()
            } else if joined.contains("get events") {
                r#"{"kind":"List","items":[{"involvedObject":{"kind":"Pod","name":"web-api-7f8b9-abc"},"metadata":{"namespace":"production"},"reason":"Started","message":"Started container api","type":"Normal","count":1,"lastTimestamp":"2026-11-05T12:00:05Z"}]}"#.to_owned()
            } else if joined.contains("logs ") {
                "2026-11-05T12:01:00Z INFO Listening on :8080\n2026-11-05T12:01:05Z ERROR Database timeout\n".to_owned()
            } else if joined.contains("describe ") {
                "Name:         web-api-7f8b9-abc\nNamespace:    production\nStatus:       Running\n".to_owned()
            } else if joined.contains("-o yaml") {
                "apiVersion: v1\nkind: Pod\nmetadata:\n  name: web-api-7f8b9-abc\n".to_owned()
            } else if joined.contains("scale ") {
                "deployment.apps/web-api scaled\n".to_owned()
            } else if joined.contains("rollout restart") {
                "deployment.apps/web-api restarted\n".to_owned()
            } else if joined.contains("apply ") {
                "deployment.apps/web-api configured\n".to_owned()
            } else if joined.contains("kustomize ") {
                "apiVersion: v1\nkind: Service\nmetadata:\n  name: kustomized-svc\n".to_owned()
            } else {
                String::new()
            };
            Ok(ProcessOutput { status: Some(0), stdout, stderr: String::new() })
        });
        host.on_process("helm", |args, _stdin| {
            let joined = args.join(" ");
            let stdout = if joined.contains("list ") {
                r#"[{"name":"ingress-nginx","namespace":"production","revision":"4","status":"deployed","chart":"ingress-nginx-4.11.0","app_version":"1.11.0"}]"#.to_owned()
            } else if joined.contains("template ") {
                "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: rendered-chart\n".to_owned()
            } else {
                "replicaCount: 2\n".to_owned()
            };
            Ok(ProcessOutput { status: Some(0), stdout, stderr: String::new() })
        });
        host
    }

    #[test]
    fn parses_multi_doc_manifests_and_navigates_cross_resource() {
        let host = MockHost::install();
        host.add_file("k8s/app.yaml", SAMPLE_MANIFEST);
        let mut plugin = KubernetesPlugin::default();
        let doc = Document {
            path: "/project/k8s/app.yaml".into(),
            language: "yaml".into(),
            version: 1,
            text: SAMPLE_MANIFEST.into(),
        };

        // Gutter line markers on ConfigMap ref, Workload exposed by Service, Service selector, Ingress backend
        let markers = plugin.line_markers(&doc);
        assert!(
            markers.len() >= 5,
            "expected at least 5 gutter markers, got {markers:?}"
        );
        assert!(
            markers
                .iter()
                .any(|m| m.tooltip.contains("Exposed by Service 'web-svc'"))
        );
        assert!(
            markers
                .iter()
                .any(|m| m.tooltip.contains("Go to matching Deployment 'web-api'"))
        );
        assert!(
            markers
                .iter()
                .any(|m| m.tooltip.contains("Go to ConfigMap 'app-config'"))
        );
        assert!(
            markers
                .iter()
                .any(|m| m.tooltip.contains("Go to key 'DATABASE_URL'"))
        );
        assert!(
            markers
                .iter()
                .any(|m| m.tooltip.contains("Go to Service 'web-svc'"))
        );

        // Go to Definition from `key: DATABASE_URL` (line 36) jumps to line 5 in ConfigMap `data:`
        let defs = plugin.definition(&doc, Position::new(36, 25));
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].range.start.line, 5);

        // Go to Definition from Service `targetPort: http` (line 56) jumps to container port `name: http` (line 29)
        let port_defs = plugin.definition(&doc, Position::new(56, 20));
        assert_eq!(port_defs.len(), 1);
        assert_eq!(port_defs[0].range.start.line, 29);
    }

    #[test]
    fn helm_values_and_named_templates_complete_and_navigate() {
        let host = MockHost::install();
        host.add_file(
            "chart/values.yaml",
            "image:\n  repository: nginx\n  tag: \"1.27.0\"\nreplicaCount: 3\n",
        );
        host.add_file(
            "chart/templates/_helpers.tpl",
            "{{- define \"mychart.fullname\" -}}\n{{ .Chart.Name }}\n{{- end -}}\n",
        );
        let tpl = "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: {{ include \"mychart.fullname\" . }}\nspec:\n  replicas: {{ .Values.replicaCount }}\n  template:\n    spec:\n      containers:\n        - name: app\n          image: \"{{ .Values.image.tag }}\"\n";
        host.add_file("chart/templates/deployment.yaml", tpl);

        let mut plugin = KubernetesPlugin::default();
        let doc = Document {
            path: "/project/chart/templates/deployment.yaml".into(),
            language: "yaml".into(),
            version: 1,
            text: tpl.into(),
        };

        // Jump from `.Values.image.tag` (line 10) to `values.yaml` line 2
        let defs = plugin.definition(&doc, Position::new(10, 32));
        assert_eq!(defs.len(), 1);
        assert!(defs[0].path.ends_with("chart/values.yaml"));
        assert_eq!(defs[0].range.start.line, 2);

        // Jump from `include "mychart.fullname"` (line 3) to `_helpers.tpl` line 0
        let tpl_defs = plugin.definition(&doc, Position::new(3, 24));
        assert_eq!(tpl_defs.len(), 1);
        assert!(tpl_defs[0].path.ends_with("chart/templates/_helpers.tpl"));
        assert_eq!(tpl_defs[0].range.start.line, 0);

        // Complete `.Values.im`
        let editing = tpl.replace(".Values.image.tag", ".Values.im");
        let edit_doc = Document {
            text: editing,
            ..doc
        };
        let items = plugin.completion(&edit_doc, Position::new(10, 29), None);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            labels.contains(&"image.tag") && labels.contains(&"image.repository"),
            "{labels:?}"
        );
    }

    #[test]
    fn inspections_and_quick_fixes_cover_deprecated_apis_labels_images_and_keys() {
        let host = MockHost::install();
        let bad_yaml = r#"apiVersion: extensions/v1beta1
kind: Deployment
metadata:
  name: legacy-app
  uid: 1234-5678
spec:
  selector:
    matchLabels:
      app: web
  template:
    metadata:
      labels:
        app: wrong-label
    spec:
      containers:
        - name: app
          image: nginx:latest
          imagePullPolicy: InvalidPolicy
          securityContext:
            privileged: true
"#;
        host.add_file("deploy.yaml", bad_yaml);
        let mut plugin = KubernetesPlugin::default();
        let count = plugin
            .check_and_publish("/project/deploy.yaml", false)
            .unwrap();
        assert!(count >= 6, "expected at least 6 diagnostics, got {count}");

        let diags = host.diagnostics();
        let file_diags = &diags["/project/deploy.yaml"];
        let codes: Vec<_> = file_diags
            .iter()
            .filter_map(|d| d.code.as_deref())
            .collect();
        assert!(codes.contains(&"deprecated-api-version"), "{codes:?}");
        assert!(codes.contains(&"selector-label-mismatch"), "{codes:?}");
        assert!(codes.contains(&"image-latest-tag"), "{codes:?}");
        assert!(codes.contains(&"missing-resource-limits"), "{codes:?}");
        assert!(codes.contains(&"privileged-container"), "{codes:?}");
        assert!(codes.contains(&"readonly-field"), "{codes:?}");
        assert!(codes.contains(&"invalid-enum-value"), "{codes:?}");

        // Quick fix on line 0 upgrades `extensions/v1beta1` to `apps/v1`
        let doc = Document {
            path: "/project/deploy.yaml".into(),
            language: "yaml".into(),
            version: 1,
            text: bad_yaml.into(),
        };
        let actions =
            plugin.code_actions(&doc, Range::new(Position::new(0, 0), Position::new(0, 10)));
        assert!(
            actions
                .iter()
                .any(|a| a.title.contains("Upgrade apiVersion to apps/v1")),
            "{actions:?}"
        );
        assert!(
            actions.iter().any(|a| a.title.contains("# nonk8s")),
            "{actions:?}"
        );
    }

    #[test]
    fn cluster_explorer_commands_and_terminal_sessions_work() {
        let host = install_mock_with_kubectl();
        host.add_file("k8s/app.yaml", SAMPLE_MANIFEST);
        host.open_editor("/project/k8s/app.yaml", "yaml", SAMPLE_MANIFEST, &[]);

        let mut plugin = KubernetesPlugin::default();
        plugin
            .activate(&host.context("dengliu.kubernetes"))
            .unwrap();

        // Render the Kubernetes Explorer view (loads kubeconfig contexts immediately)
        let view = plugin.render(VIEW_ID, None, &host.invocation()).unwrap();
        let Body::Html(html) = &view.body else {
            panic!("expected Html view")
        };
        assert!(!html.contains("__START_B64__"));
        assert_eq!(
            host.status_items()["context"].text,
            "k8s: prod-east/production"
        );

        // Page requests cluster resources via `{op: "refresh"}`
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(r#"{"op":"refresh"}"#.into()),
                &host.invocation(),
            )
            .unwrap();
        assert!(
            host.view_messages()
                .iter()
                .any(|(v, m)| v == VIEW_ID && m.contains("web-api-7f8b9-abc"))
        );

        // Switch context command cycles to `dev-local`
        let next_ctx = plugin
            .run_command("switch-context", &host.invocation(), None)
            .unwrap()
            .unwrap();
        assert_eq!(next_ctx, "dev-local");
        assert_eq!(
            host.storage().get("context").map(String::as_str),
            Some("dev-local")
        );

        // Follow logs & Exec shell open terminals with the right kubectl arguments
        plugin
            .run_command(
                "follow-logs",
                &host.invocation(),
                Some("production/web-api-7f8b9-abc/api"),
            )
            .unwrap();
        plugin
            .run_command(
                "exec-shell",
                &host.invocation(),
                Some("production/web-api-7f8b9-abc/api"),
            )
            .unwrap();
        plugin
            .run_command(
                "port-forward",
                &host.invocation(),
                Some("Service/production/web-svc:8080:80"),
            )
            .unwrap();
        let terms = host.terminals();
        assert_eq!(terms.len(), 3);
        assert!(terms[0].args.contains(&"logs".into()) && terms[0].args.contains(&"-f".into()));
        assert!(terms[1].args.contains(&"exec".into()) && terms[1].args.contains(&"-it".into()));
        assert!(
            terms[2].args.contains(&"port-forward".into())
                && terms[2].args.contains(&"8080:80".into())
        );

        // Scale and Rollout Restart commands
        let scaled = plugin
            .run_command(
                "scale",
                &host.invocation(),
                Some("Deployment/production/web-api=4"),
            )
            .unwrap()
            .unwrap();
        assert!(scaled.contains("scaled"));
        let restarted = plugin
            .run_command(
                "rollout-restart",
                &host.invocation(),
                Some("Deployment/production/web-api"),
            )
            .unwrap()
            .unwrap();
        assert!(restarted.contains("restarted"));

        // Kustomize and Helm Template commands
        let kust = plugin
            .run_command("kustomize-build", &host.invocation(), Some("."))
            .unwrap()
            .unwrap();
        assert!(kust.contains("kustomized-svc"));
        let helm_out = plugin
            .run_command("helm-template", &host.invocation(), Some("."))
            .unwrap()
            .unwrap();
        assert!(helm_out.contains("rendered-chart"));

        // Live YAML editing in the tool window (`apply-yaml`)
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"apply-yaml","yaml":"apiVersion: v1\nkind: Pod\nmetadata:\n  name: web-api-7f8b9-abc\n"}"#
                        .into(),
                ),
                &host.invocation(),
            )
            .unwrap();
        assert!(
            host.view_messages()
                .iter()
                .any(|(v, m)| v == VIEW_ID && m.contains("Applied to cluster"))
        );

        // Content editor tab round-trip (`open-resource-yaml` -> `content-tab-action` reload/diff/save)
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"open-resource-yaml","key":"k8s-yaml:dev-local|production|Pod|web-api-7f8b9-abc","title":"Pod: web-api-7f8b9-abc.yaml","context":"dev-local","namespace":"production","kind":"Pod","name":"web-api-7f8b9-abc"}"#
                        .into(),
                ),
                &host.invocation(),
            )
            .unwrap();
        assert!(
            host.view_messages()
                .iter()
                .any(|(v, m)| v == VIEW_ID && m.contains("resource-yaml-ready"))
        );

        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"content-tab-action","requestId":"ct-1","key":"k8s-yaml:dev-local|production|Pod|web-api-7f8b9-abc","action":"diff","content":"apiVersion: v1\nkind: Pod\nmetadata:\n  name: web-api-7f8b9-abc\n"}"#
                        .into(),
                ),
                &host.invocation(),
            )
            .unwrap();
        assert!(host.view_messages().iter().any(|(v, m)| v == VIEW_ID
            && m.contains(r#""op":"content-tab-reply""#)
            && m.contains(r#""requestId":"ct-1""#)));

        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"content-tab-action","requestId":"ct-2","key":"k8s-yaml:dev-local|production|Pod|web-api-7f8b9-abc","action":"save","content":"apiVersion: v1\nkind: Pod\nmetadata:\n  name: web-api-7f8b9-abc\n"}"#
                        .into(),
                ),
                &host.invocation(),
            )
            .unwrap();
        assert!(host.view_messages().iter().any(|(v, m)| v == VIEW_ID
            && m.contains(r#""requestId":"ct-2""#)
            && m.contains(r#""saved":true"#)));
    }

    #[test]
    fn merges_freelens_cluster_store_and_hotbars() {
        let (_, _, _, mut catalog) = kubectl::parse_kubeconfig_catalog(
            r#"{
                "current-context": "falcon-phx-ga",
                "clusters": [{"name": "falcon-phx-ga", "cluster": {"server": "https://phx-ga.example.com"}}],
                "contexts": [
                    {"name": "falcon-phx-ga", "context": {"cluster": "falcon-phx-ga", "namespace": "default"}},
                    {"name": "falcon-dal-ca", "context": {"cluster": "falcon-dal-ca"}}
                ]
            }"#,
            "",
            "~/.kube/config",
        );
        let clusters_json = r#"{
            "clusters": [
                {
                    "id": "d1387eed1a8f84eba2adb853dd0c0d0f",
                    "contextName": "falcon-phx-ga",
                    "kubeConfigPath": "/Users/dengliu/.kube/config",
                    "metadata": {"version": "v1.36.3", "distribution": "aks", "nodes": 132}
                },
                {
                    "id": "f78a9808",
                    "contextName": "falcon-dal-ca",
                    "kubeConfigPath": "/Users/dengliu/.kube/config",
                    "metadata": {"version": "v1.36.2", "distribution": "unknown", "nodes": 545}
                }
            ]
        }"#;
        let hotbars_json = r#"{
            "hotbars": [
                {
                    "items": [
                        {"entity": {"uid": "d1387eed1a8f84eba2adb853dd0c0d0f", "name": "falcon-phx-ga", "shortName": "fpg"}}
                    ]
                }
            ]
        }"#;
        kubectl::merge_freelens_stores(&mut catalog, Some(clusters_json), Some(hotbars_json));
        assert_eq!(catalog[0].version, "v1.36.3");
        assert_eq!(catalog[0].distro, "aks");
        assert_eq!(catalog[0].nodes, Some(132));
        assert!(catalog[0].hotbar_pinned);
        assert_eq!(catalog[0].short_name, "FPG");
        assert_eq!(catalog[1].version, "v1.36.2");
        assert_eq!(catalog[1].short_name, "FDC");
        assert!(!catalog[1].hotbar_pinned);
    }

    #[test]
    fn poll_category_attaches_bounded_request_timeout_and_reports_completion() {
        let seen_args = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        let host = install_mock_with_kubectl();
        let seen_clone = seen_args.clone();
        host.on_process("kubectl", move |args, _stdin| {
            seen_clone.lock().unwrap().push(args.to_vec());
            let joined = args.join(" ");
            let stdout = if joined.contains("config view") {
                KUBECONFIG_JSON.to_owned()
            } else if joined.contains("get pods") {
                RESOURCES_JSON.to_owned()
            } else {
                String::new()
            };
            Ok(ProcessOutput {
                status: Some(0),
                stdout,
                stderr: String::new(),
            })
        });
        host.add_file("k8s/app.yaml", SAMPLE_MANIFEST);

        let mut plugin = KubernetesPlugin::default();
        plugin
            .activate(&host.context("dengliu.kubernetes"))
            .unwrap();
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"poll-category","category":"Pod","includeMetrics":false}"#.into(),
                ),
                &host.invocation(),
            )
            .unwrap();

        let recorded = seen_args.lock().unwrap();
        let pod_call = recorded
            .iter()
            .find(|a| a.iter().any(|x| x == "pods"))
            .expect("pod poll call");
        assert!(
            pod_call.contains(&"--request-timeout=3s".to_owned()),
            "{pod_call:?}"
        );
        assert!(
            !recorded.iter().any(|a| a.iter().any(|x| x == "top")),
            "metrics skipped when includeMetrics=false"
        );
        assert!(host.view_messages().iter().any(|(v, m)| v == VIEW_ID
            && m.contains(r#""pollCompleted":true"#)
            && m.contains(r#""pollDurationMs":"#)));
    }

    #[test]
    fn parses_server_side_tables_for_large_clusters_without_full_json() {
        let pods_wide = "\
kube-system   coredns-7c65d6cfc9-x8p4k   1/1   Running            0               42d   10.244.0.12   aks-sys-01   <none>   <none>
train-jobs    trainer-worker-0           1/2   CrashLoopBackOff   4 (2m15s ago)   18m   10.244.8.44   aks-gpu-99   <none>   <none>
batch         migrate-db-x9z             0/1   Completed          0               5h    10.244.9.10   aks-cpu-03   <none>   <none>
";
        let pods = kubectl::parse_pods_wide_table(pods_wide, true, "default");
        assert_eq!(pods.len(), 3);
        assert_eq!(pods[0].namespace, "kube-system");
        assert_eq!(pods[0].ready, "1/1");
        assert_eq!(pods[0].pod_ip, "10.244.0.12");
        assert_eq!(pods[0].node_name, "aks-sys-01");
        assert_eq!(pods[1].restarts, 4);
        assert_eq!(pods[1].age, "18m");
        assert_eq!(pods[1].pod_ip, "10.244.8.44");
        assert_eq!(pods[1].container_states.len(), 2);
        assert!(pods[1].container_states[0].ready && !pods[1].container_states[1].ready);
        assert_eq!(pods[2].container_states[0].state, "terminated");

        let nodes_wide = "\
aks-gpu-001   Ready                      agent           31d   v1.31.2   10.10.1.4   <none>   Ubuntu 22.04.5 LTS   6.8.0-1017-azure   containerd://1.7.22
aks-gpu-002   Ready,SchedulingDisabled   control-plane   31d   v1.31.2   10.10.1.5   <none>   Ubuntu 22.04.5 LTS   6.8.0-1017-azure   containerd://1.7.22
";
        let nodes = kubectl::parse_nodes_wide_table(nodes_wide, &std::collections::BTreeMap::new());
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].qos, "True");
        assert_eq!(nodes[0].ready, "v1.31.2");
        assert_eq!(nodes[0].pod_ip, "10.10.1.4");
        assert_eq!(nodes[0].node_name, "6.8.0-1017-azure");
        assert_eq!(nodes[1].qos, "False");
        assert_eq!(nodes[1].host_ip, "control-plane");

        let deploys = kubectl::parse_server_table_resources(
            "Deployment",
            "production   web-api   3/3   3   3   12d\nstaging      canary    1/2   2   1   45m\n",
            true,
            "default",
        );
        assert_eq!(deploys.len(), 2);
        assert_eq!(deploys[0].status, "Ready");
        assert_eq!(deploys[0].age, "12d");
        assert_eq!(deploys[1].status, "Progressing");
    }

    #[test]
    fn large_cluster_nodes_skip_client_side_custom_columns() {
        let seen_args = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        let host = install_mock_with_kubectl();
        let seen_clone = seen_args.clone();
        host.on_process("kubectl", move |args, _stdin| {
            seen_clone.lock().unwrap().push(args.to_vec());
            let joined = args.join(" ");
            let stdout = if joined.contains("config view") {
                KUBECONFIG_JSON.to_owned()
            } else if joined.contains("get nodes -o wide") {
                (0..120)
                    .map(|i| {
                        format!(
                            "aks-node-{i:03} Ready agent 14d v1.31.2 10.0.0.{} <none> Ubuntu 22.04 6.8.0-azure containerd://1.7",
                            (i % 250) + 1
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                String::new()
            };
            Ok(ProcessOutput {
                status: Some(0),
                stdout,
                stderr: String::new(),
            })
        });
        host.add_file("k8s/app.yaml", SAMPLE_MANIFEST);

        let mut plugin = KubernetesPlugin::default();
        plugin
            .activate(&host.context("dengliu.kubernetes"))
            .unwrap();
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(r#"{"op":"load-category","category":"Node"}"#.into()),
                &host.invocation(),
            )
            .unwrap();

        let recorded = seen_args.lock().unwrap();
        assert!(
            !recorded
                .iter()
                .any(|a| a.iter().any(|x| x.starts_with("custom-columns="))),
            "large >80-node clusters must not fetch full Node JSON via custom-columns: {recorded:?}"
        );
        assert_eq!(
            plugin
                .snapshot
                .as_ref()
                .unwrap()
                .resources
                .iter()
                .filter(|r| r.kind == "Node")
                .count(),
            120
        );
    }

    #[test]
    fn enforces_large_cluster_scope_and_caps_initial_html_payload() {
        let host = install_mock_with_kubectl();
        let mut plugin = KubernetesPlugin::default();
        plugin
            .activate(&host.context("dengliu.kubernetes"))
            .unwrap();

        let namespaces: Vec<String> = (0..220).map(|i| format!("team-ns-{i:03}")).collect();
        let resources: Vec<kubectl::ClusterResource> = (0..1200)
            .map(|i| kubectl::ClusterResource {
                kind: "Pod".into(),
                name: format!("pod-{i:04}"),
                namespace: "team-ns-001".into(),
                status: "Running".into(),
                ready: "1/1".into(),
                ..Default::default()
            })
            .collect();
        let mut snap = ClusterSnapshot {
            connected: true,
            current_context: "prod-east".into(),
            current_namespace: "team-ns-001".into(),
            all_namespaces: true,
            namespaces,
            resources,
            ..Default::default()
        };
        let mut cfg = KubeConfigState {
            all_namespaces: true,
            namespace: "team-ns-001".into(),
            ..Default::default()
        };
        assert!(kubectl::enforce_large_cluster_scope(&mut cfg, &mut snap));
        assert!(!cfg.all_namespaces && !snap.all_namespaces);

        plugin.snapshot = Some(snap);
        let view = plugin.render(VIEW_ID, None, &host.invocation()).unwrap();
        let Body::Html(html) = &view.body else {
            panic!("expected Html view");
        };
        assert!(
            html.len() < 3 * 1024 * 1024 && html.trim_end().ends_with("</script>"),
            "rendered HTML must stay well below 4 MiB MAX_HTML without truncating closing script tag (len={})",
            html.len()
        );
    }

    #[test]
    fn server_side_limit_and_continue_pagination_works_across_pods_nodes_events_and_crs() {
        let seen_args = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        let host = install_mock_with_kubectl();
        let seen_clone = seen_args.clone();
        host.on_process("kubectl", move |args, _stdin| {
            seen_clone.lock().unwrap().push(args.to_vec());
            let joined = args.join(" ");
            let stdout = if joined.contains("config view") {
                KUBECONFIG_JSON.to_owned()
            } else if joined.contains("get namespaces --no-headers") {
                "default Active 30d\nproduction Active 30d\n".to_owned()
            } else if joined.contains("get crd --no-headers") {
                "workloads.kueue.x-k8s.io 2025-07-29T21:30:07Z\n".to_owned()
            } else if joined.contains("--raw /apis/kueue.x-k8s.io/v1beta1/namespaces/production/workloads?limit=50") {
                r#"{"kind":"WorkloadList","metadata":{"continue":"cr-tok-2","remainingItemCount":850},"items":[{"metadata":{"name":"job-wl-1","namespace":"production","creationTimestamp":"2026-11-11T10:00:00Z"},"status":{"phase":"Admitted"}}]}"#.to_owned()
            } else if joined.contains("--raw /apis/kueue.x-k8s.io/v1beta1") {
                r#"{"kind":"APIResourceList","resources":[{"name":"workloads","namespaced":true,"kind":"Workload"}]}"#.to_owned()
            } else if joined.contains("--raw /apis/kueue.x-k8s.io") {
                r#"{"kind":"APIGroup","preferredVersion":{"version":"v1beta1"}}"#.to_owned()
            } else if joined.contains("pods?limit=50&continue=expired-tok") {
                return Ok(ProcessOutput {
                    status: Some(1),
                    stdout: String::new(),
                    stderr: "Error from server (Gone): 410: continue token is too old (ResourceExpired)".into(),
                });
            } else if joined.contains("pods?limit=50&continue=pod-tok-2") {
                r#"{"kind":"PodList","metadata":{"continue":"pod-tok-3","remainingItemCount":52710},"items":[{"metadata":{"name":"worker-002","namespace":"train","creationTimestamp":"2026-11-11T10:00:00Z"},"spec":{"nodeName":"gpu-02","containers":[{"name":"app"}]},"status":{"phase":"Running","podIP":"10.1.0.2","containerStatuses":[{"name":"app","ready":true,"restartCount":0,"state":{"running":{"startedAt":"2026-11-11T10:00:01Z"}}}]}}]}"#.to_owned()
            } else if joined.contains("pods?limit=50") {
                r#"{"kind":"PodList","metadata":{"continue":"pod-tok-2","remainingItemCount":52760},"items":[{"metadata":{"name":"worker-001","namespace":"train","creationTimestamp":"2026-11-11T10:00:00Z"},"spec":{"nodeName":"gpu-01","containers":[{"name":"app"}]},"status":{"phase":"Running","podIP":"10.1.0.1","containerStatuses":[{"name":"app","ready":true,"restartCount":1,"state":{"running":{"startedAt":"2026-11-11T10:00:01Z"}}}]}}]}"#.to_owned()
            } else if joined.contains("--raw /api/v1/nodes?limit=50") {
                r#"{"kind":"NodeList","metadata":{"continue":"node-tok-2","remainingItemCount":2123},"items":[{"metadata":{"name":"aks-gpu-001","creationTimestamp":"2026-10-01T00:00:00Z","labels":{"node-role.kubernetes.io/gpu":"","node.kubernetes.io/instance-type":"Standard_ND96amsr_A100_v4","kubernetes.azure.com/agentpool":"gpupool"}},"spec":{"unschedulable":true,"taints":[{"key":"nvidia.com/gpu","value":"present","effect":"NoSchedule"}]},"status":{"allocatable":{"cpu":"96","memory":"1900Gi"},"addresses":[{"type":"InternalIP","address":"10.240.0.4"}],"conditions":[{"type":"Ready","status":"True"}],"nodeInfo":{"kubeletVersion":"v1.31.2","kernelVersion":"6.8.0-1017-azure"}}}]}"#.to_owned()
            } else if joined.contains("events?limit=50") {
                r#"{"kind":"EventList","metadata":{"continue":"ev-tok-2","remainingItemCount":133700},"items":[{"metadata":{"namespace":"production","creationTimestamp":"2026-11-11T12:00:00Z"},"involvedObject":{"kind":"Pod","name":"worker-001"},"reason":"Pulled","message":"Successfully pulled image","type":"Normal","count":1}]}"#.to_owned()
            } else {
                String::new()
            };
            Ok(ProcessOutput {
                status: Some(0),
                stdout,
                stderr: String::new(),
            })
        });
        host.add_file("k8s/app.yaml", SAMPLE_MANIFEST);

        let mut plugin = KubernetesPlugin::default();
        plugin
            .activate(&host.context("dengliu.kubernetes"))
            .unwrap();

        // 1. All-namespaces Pods page 1 (`limit=50`): returns page 1 + continue token, skips `kubectl top pods` because total > 300
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"set-all-namespaces","all":true,"category":"Pod","limit":50}"#.into(),
                ),
                &host.invocation(),
            )
            .unwrap();
        {
            let snap = plugin.snapshot.as_ref().unwrap();
            let pi = snap.page_info.get("Pod").expect("Pod page_info");
            assert_eq!(pi.continue_token, "pod-tok-2");
            assert_eq!(pi.remaining_count, Some(52_760));
            assert_eq!(snap.overview_counts.get("Pod"), Some(&52_761));
            assert_eq!(snap.resources.iter().filter(|r| r.kind == "Pod").count(), 1);
            assert!(
                !seen_args
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|a| a.iter().any(|x| x == "top")),
                "must skip cluster-wide kubectl top pods when total_pods > 300"
            );
        }

        // 2. Append mode ("Load more") appends page 2 without dropping page 1
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"load-category","category":"Pod","limit":50,"continueToken":"pod-tok-2","pageMode":"append","pageIndex":2}"#.into(),
                ),
                &host.invocation(),
            )
            .unwrap();
        {
            let snap = plugin.snapshot.as_ref().unwrap();
            let pods: Vec<_> = snap.resources.iter().filter(|r| r.kind == "Pod").collect();
            assert_eq!(pods.len(), 2);
            assert_eq!(pods[0].name, "worker-001");
            assert_eq!(pods[1].name, "worker-002");
            assert_eq!(snap.page_info["Pod"].continue_token, "pod-tok-3");
            assert_eq!(snap.page_info["Pod"].mode, "append");
        }

        // 3. Expired continue token (410 Gone) recovers cleanly back to Page 1
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"load-category","category":"Pod","limit":50,"continueToken":"expired-tok","pageMode":"replace","pageIndex":4}"#.into(),
                ),
                &host.invocation(),
            )
            .unwrap();
        {
            let snap = plugin.snapshot.as_ref().unwrap();
            assert_eq!(snap.page_info["Pod"].page_index, 1);
            assert_eq!(snap.page_info["Pod"].continue_token, "pod-tok-2");
        }

        // 4. Nodes paged JSON populates full Freelens Node columns
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(r#"{"op":"load-category","category":"Node","limit":50}"#.into()),
                &host.invocation(),
            )
            .unwrap();
        {
            let snap = plugin.snapshot.as_ref().unwrap();
            let node = snap
                .resources
                .iter()
                .find(|r| r.kind == "Node")
                .expect("Node row");
            assert_eq!(node.host_ip, "gpu");
            assert_eq!(node.qos, "False");
            assert_eq!(node.status, "Ready, SchedulingDisabled");
            assert_eq!(node.pod_ip, "10.240.0.4");
            assert_eq!(node.ready, "v1.31.2");
            assert_eq!(node.node_name, "6.8.0-1017-azure");
            assert_eq!(node.controlled_by_kind, "Standard_ND96amsr_A100_v4");
            assert_eq!(node.controlled_by_name, "gpupool");
            assert_eq!(node.restarts, 1);
            assert_eq!(node.cpu, "0.00/96.00");
            assert_eq!(snap.page_info["Node"].remaining_count, Some(2123));
        }

        // 5. Events & Custom Resources (`CR:*`) paginate via REST API
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"set-namespace","namespace":"production","category":"Event","limit":50}"#.into(),
                ),
                &host.invocation(),
            )
            .unwrap();
        assert_eq!(
            plugin.snapshot.as_ref().unwrap().page_info["Event"].remaining_count,
            Some(133_700)
        );

        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"load-category","category":"CR:workloads.kueue.x-k8s.io","limit":50}"#
                        .into(),
                ),
                &host.invocation(),
            )
            .unwrap();
        {
            let snap = plugin.snapshot.as_ref().unwrap();
            let cr = snap
                .resources
                .iter()
                .find(|r| r.kind == "CR:workloads.kueue.x-k8s.io")
                .expect("CR row");
            assert_eq!(cr.name, "job-wl-1");
            assert_eq!(cr.status, "Admitted");
            assert_eq!(
                snap.page_info["CR:workloads.kueue.x-k8s.io"].continue_token,
                "cr-tok-2"
            );
        }
    }

    #[test]
    fn hybrid_watch_pipeline_parses_frames_and_switches_by_scope_size() {
        // 1. Verify frame parsing for ADDED, MODIFIED, DELETED, BOOKMARK, and 410 ERROR
        let mod_line = r#"{"type":"MODIFIED","object":{"kind":"Pod","metadata":{"name":"api-0","namespace":"production","resourceVersion":"9002","creationTimestamp":"2026-11-11T10:00:00Z"},"spec":{"nodeName":"node-1","containers":[{"name":"web"}]},"status":{"phase":"Running","podIP":"10.2.0.9","containerStatuses":[{"name":"web","ready":true,"restartCount":2,"state":{"running":{"startedAt":"2026-11-11T10:01:00Z"}}}]}}}"#;
        match kubectl::parse_watch_frame_line(mod_line, "Pod") {
            ParsedWatchFrame::ResourceUpsert {
                resource,
                resource_version,
                ..
            } => {
                assert_eq!(resource_version, "9002");
                assert_eq!(resource.name, "api-0");
                assert_eq!(resource.namespace, "production");
                assert_eq!(resource.restarts, 2);
                assert_eq!(resource.pod_ip, "10.2.0.9");
            }
            other => panic!("unexpected frame: {other:?}"),
        }

        let del_line = r#"{"type":"DELETED","object":{"metadata":{"name":"api-0","namespace":"production","resourceVersion":"9003"}}}"#;
        assert_eq!(
            kubectl::parse_watch_frame_line(del_line, "Pod"),
            ParsedWatchFrame::ResourceDeleted {
                kind: "Pod".into(),
                namespace: "production".into(),
                name: "api-0".into(),
                resource_version: "9003".into(),
            }
        );

        let bmk_line =
            r#"{"type":"BOOKMARK","object":{"kind":"Pod","metadata":{"resourceVersion":"9050"}}}"#;
        assert_eq!(
            kubectl::parse_watch_frame_line(bmk_line, "Pod"),
            ParsedWatchFrame::Bookmark {
                resource_version: "9050".into()
            }
        );

        let gone_line = r#"{"type":"ERROR","object":{"kind":"Status","code":410,"reason":"Expired","message":"too old resource version: 8000 (9050)"}}"#;
        assert!(matches!(
            kubectl::parse_watch_frame_line(gone_line, "Pod"),
            ParsedWatchFrame::Expired { .. }
        ));

        // 2. Verify hybrid eligibility: single-namespace <= 1,000 items starts category watch,
        //    while > 1,000 items disables category watch yet keeps single-object Drawer watch active.
        let host = install_mock_with_kubectl();
        host.on_process("kubectl", move |args, _stdin| {
            let joined = args.join(" ");
            let stdout = if joined.contains("config view") {
                KUBECONFIG_JSON.to_owned()
            } else if joined.contains("get namespaces --no-headers") {
                "default Active 30d\nproduction Active 30d\n".to_owned()
            } else if joined.contains("/api/v1/namespaces/production/pods?limit=100") {
                r#"{"kind":"PodList","metadata":{"resourceVersion":"7701"},"items":[{"metadata":{"name":"web-0","namespace":"production","resourceVersion":"7700","creationTimestamp":"2026-11-11T10:00:00Z"},"spec":{"containers":[{"name":"app"}]},"status":{"phase":"Running"}}]}"#.to_owned()
            } else if joined.contains("/api/v1/pods?limit=100") {
                r#"{"kind":"PodList","metadata":{"resourceVersion":"8801","continue":"tok-2","remainingItemCount":52000},"items":[{"metadata":{"name":"huge-0","namespace":"ns1","creationTimestamp":"2026-11-11T10:00:00Z"},"status":{"phase":"Running"}}]}"#.to_owned()
            } else if joined.contains("get pod huge-0 -o json") {
                r#"{"kind":"Pod","metadata":{"name":"huge-0","namespace":"ns1","resourceVersion":"8805"},"status":{"phase":"Running"}}"#.to_owned()
            } else {
                String::new()
            };
            Ok(ProcessOutput {
                status: Some(0),
                stdout,
                stderr: String::new(),
            })
        });
        host.add_file("k8s/app.yaml", SAMPLE_MANIFEST);

        let mut plugin = KubernetesPlugin::default();
        plugin
            .activate(&host.context("dengliu.kubernetes"))
            .unwrap();

        // Small single-namespace scope (`production`): `watch_supported == true` and category watch stream is started
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"set-namespace","namespace":"production","category":"Pod","limit":100}"#
                        .into(),
                ),
                &host.invocation(),
            )
            .unwrap();
        assert!(plugin.snapshot.as_ref().unwrap().page_info["Pod"].watch_supported);
        assert!(!plugin.watches.category_streams.is_empty());

        // Switching to All Namespaces (52,001 pods > 1,000): stops category watch & uses paged LIST
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"set-all-namespaces","all":true,"category":"Pod","limit":100}"#.into(),
                ),
                &host.invocation(),
            )
            .unwrap();
        assert!(!plugin.snapshot.as_ref().unwrap().page_info["Pod"].watch_supported);
        assert!(plugin.watches.category_streams.is_empty());

        // Inspecting one Pod in the Drawer starts a single-object fieldSelector watch even on the 52k cluster
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"inspect-json","kind":"Pod","namespace":"ns1","name":"huge-0"}"#
                        .into(),
                ),
                &host.invocation(),
            )
            .unwrap();
        assert!(plugin.watches.drawer_stream.is_some());
        assert_eq!(
            plugin
                .watches
                .drawer_stream
                .as_ref()
                .unwrap()
                .spec
                .field_selector_name
                .as_deref(),
            Some("huge-0")
        );

        // `poll-watch` emits a `watch-delta` message without stopping the drawer stream
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"poll-watch","category":"Pod","inspectKind":"Pod","inspectNs":"ns1","inspectName":"huge-0"}"#
                        .into(),
                ),
                &host.invocation(),
            )
            .unwrap();
        assert!(host.view_messages().iter().any(|(v, m)| v == VIEW_ID
            && m.contains(r#""op":"watch-delta""#)
            && m.contains(r#""drawerWatching":true"#)));
    }

    #[test]
    fn multi_namespace_selection_paginates_and_watches_across_namespaces() {
        let host = install_mock_with_kubectl();
        host.on_process("kubectl", move |args, _stdin| {
            let joined = args.join(" ");
            let stdout = if joined.contains("config view") {
                KUBECONFIG_JSON.to_owned()
            } else if joined.contains("get namespaces --no-headers") {
                "dayan Active 30d\ndengliu Active 30d\nteam-large Active 30d\n".to_owned()
            } else if joined.contains("/api/v1/namespaces/dayan/pods?limit=50") {
                r#"{"kind":"PodList","metadata":{"resourceVersion":"4101"},"items":[{"metadata":{"name":"brix-git","namespace":"dayan","resourceVersion":"4100","creationTimestamp":"2026-11-11T10:00:00Z"},"spec":{"containers":[{"name":"git"}]},"status":{"phase":"Running"}}]}"#.to_owned()
            } else if joined.contains("/api/v1/namespaces/dengliu/pods?limit=50") {
                r#"{"kind":"PodList","metadata":{"resourceVersion":"5202"},"items":[{"metadata":{"name":"dev-pod-0","namespace":"dengliu","resourceVersion":"5201","creationTimestamp":"2026-11-11T10:05:00Z"},"spec":{"containers":[{"name":"dev"}]},"status":{"phase":"Running"}}]}"#.to_owned()
            } else if joined.contains("/api/v1/namespaces/team-large/pods?limit=10&continue=tl-2") {
                r#"{"kind":"PodList","metadata":{"resourceVersion":"6309"},"items":[{"metadata":{"name":"train-1","namespace":"team-large","creationTimestamp":"2026-11-11T10:10:00Z"},"status":{"phase":"Running"}}]}"#.to_owned()
            } else if joined.contains("/api/v1/namespaces/team-large/pods?limit=") {
                r#"{"kind":"PodList","metadata":{"resourceVersion":"6305","continue":"tl-2","remainingItemCount":600},"items":[{"metadata":{"name":"train-0","namespace":"team-large","creationTimestamp":"2026-11-11T10:09:00Z"},"status":{"phase":"Running"}}]}"#.to_owned()
            } else if joined.contains("/api/v1/namespaces/dayan/pods?limit=10")
                || joined.contains("/api/v1/namespaces/dayan/pods?limit=")
            {
                r#"{"kind":"PodList","metadata":{"resourceVersion":"4101"},"items":[{"metadata":{"name":"brix-git","namespace":"dayan","creationTimestamp":"2026-11-11T10:00:00Z"},"status":{"phase":"Running"}}]}"#.to_owned()
            } else {
                String::new()
            };
            Ok(ProcessOutput {
                status: Some(0),
                stdout,
                stderr: String::new(),
            })
        });
        host.add_file("k8s/app.yaml", SAMPLE_MANIFEST);

        let mut plugin = KubernetesPlugin::default();
        plugin
            .activate(&host.context("dengliu.kubernetes"))
            .unwrap();

        // 1. Selecting 2 small namespaces (`dayan` + `dengliu`) merges pods & opens 2 live watch streams
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"set-namespaces","namespaces":["dayan","dengliu"],"category":"Pod","limit":50}"#
                        .into(),
                ),
                &host.invocation(),
            )
            .unwrap();

        let snap = plugin.snapshot.as_ref().unwrap();
        assert!(!snap.all_namespaces);
        assert_eq!(snap.selected_namespaces, vec!["dayan", "dengliu"]);
        assert_eq!(snap.current_namespace, "dayan");
        let pod_names: Vec<(&str, &str)> = snap
            .resources
            .iter()
            .filter(|r| r.kind == "Pod")
            .map(|r| (r.namespace.as_str(), r.name.as_str()))
            .collect();
        assert_eq!(
            pod_names,
            vec![("dayan", "brix-git"), ("dengliu", "dev-pod-0")]
        );
        assert!(snap.page_info["Pod"].watch_supported);
        assert_eq!(plugin.watches.category_streams.len(), 2);

        // 2. Paginating across `["team-large", "dayan"]` when `team-large` emits a `continue` cursor (`mns:0:tl-2`)
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"set-namespaces","namespaces":["team-large","dayan"],"category":"Pod","limit":10}"#
                        .into(),
                ),
                &host.invocation(),
            )
            .unwrap();
        let snap_p1 = plugin.snapshot.as_ref().unwrap();
        assert_eq!(snap_p1.page_info["Pod"].continue_token, "mns:0:tl-2");
        assert!(!snap_p1.page_info["Pod"].watch_supported);

        // Advancing with `continueToken: "mns:0:tl-2"` finishes `team-large` (`train-1`) and continues into `dayan` (`brix-git`)!
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"load-category","category":"Pod","limit":10,"continueToken":"mns:0:tl-2","pageMode":"replace","pageIndex":2}"#
                        .into(),
                ),
                &host.invocation(),
            )
            .unwrap();
        let snap_p2 = plugin.snapshot.as_ref().unwrap();
        assert_eq!(snap_p2.page_info["Pod"].continue_token, "");
        let p2_names: Vec<(&str, &str)> = snap_p2
            .resources
            .iter()
            .filter(|r| r.kind == "Pod")
            .map(|r| (r.namespace.as_str(), r.name.as_str()))
            .collect();
        assert_eq!(
            p2_names,
            vec![("team-large", "train-1"), ("dayan", "brix-git")]
        );
    }

    #[test]
    fn scopes_up_to_500_items_auto_drain_completely_and_enable_live_watch() {
        let host = install_mock_with_kubectl();
        host.on_process("kubectl", move |args, _stdin| {
            let joined = args.join(" ");
            let stdout = if joined.contains("config view") {
                KUBECONFIG_JSON.to_owned()
            } else if joined.contains("get namespaces --no-headers") {
                "default Active 30d\nmedium-ns Active 30d\nlarge-ns Active 30d\n".to_owned()
            } else if joined.contains("/api/v1/namespaces/medium-ns/services?limit=250&continue=svc-p2") {
                r#"{"kind":"ServiceList","metadata":{"resourceVersion":"9105"},"items":[{"metadata":{"name":"svc-b","namespace":"medium-ns","creationTimestamp":"2026-11-12T10:00:00Z"},"spec":{"type":"ClusterIP"}}]}"#.to_owned()
            } else if joined.contains("/api/v1/namespaces/medium-ns/services?limit=200") {
                // 1 + 319 remaining = 320 <= 500 -> should auto-drain `svc-p2` right away!
                r#"{"kind":"ServiceList","metadata":{"resourceVersion":"9100","continue":"svc-p2","remainingItemCount":319},"items":[{"metadata":{"name":"svc-a","namespace":"medium-ns","creationTimestamp":"2026-11-12T09:59:00Z"},"spec":{"type":"ClusterIP"}}]}"#.to_owned()
            } else if joined.contains("/api/v1/namespaces/large-ns/services?limit=200") {
                // 1 + 520 remaining = 521 > 500 -> stays paged with `continue_token = "svc-big-2"` and `watch_supported = false`
                r#"{"kind":"ServiceList","metadata":{"resourceVersion":"9200","continue":"svc-big-2","remainingItemCount":520},"items":[{"metadata":{"name":"svc-0","namespace":"large-ns","creationTimestamp":"2026-11-12T09:59:00Z"},"spec":{"type":"ClusterIP"}}]}"#.to_owned()
            } else {
                String::new()
            };
            Ok(ProcessOutput {
                status: Some(0),
                stdout,
                stderr: String::new(),
            })
        });
        host.add_file("k8s/app.yaml", SAMPLE_MANIFEST);

        let mut plugin = KubernetesPlugin::default();
        plugin
            .activate(&host.context("dengliu.kubernetes"))
            .unwrap();

        // 1. `medium-ns` (<= 500 items): drains continuation automatically, loads all items, starts live watch
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"set-namespace","namespace":"medium-ns","category":"Service","limit":200}"#
                        .into(),
                ),
                &host.invocation(),
            )
            .unwrap();
        let snap_med = plugin.snapshot.as_ref().unwrap();
        let svc_names: Vec<&str> = snap_med
            .resources
            .iter()
            .filter(|r| r.kind == "Service")
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(svc_names, vec!["svc-a", "svc-b"]);
        assert_eq!(snap_med.page_info["Service"].continue_token, "");
        assert!(snap_med.page_info["Service"].watch_supported);
        assert_eq!(plugin.watches.category_streams.len(), 1);

        // 2. `large-ns` (521 > 500 items): keeps `continue_token` for virtualized auto-scroll and stops watch
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"set-namespace","namespace":"large-ns","category":"Service","limit":200}"#
                        .into(),
                ),
                &host.invocation(),
            )
            .unwrap();
        let snap_lg = plugin.snapshot.as_ref().unwrap();
        assert_eq!(snap_lg.page_info["Service"].continue_token, "svc-big-2");
        assert!(!snap_lg.page_info["Service"].watch_supported);
        assert!(plugin.watches.category_streams.is_empty());
    }

    #[test]
    fn ground_truth_cluster_search_finds_items_beyond_loaded_page_and_preserves_base_snapshot() {
        let seen_args = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        let seen_clone = seen_args.clone();
        let host = install_mock_with_kubectl();
        host.on_process("kubectl", move |args, _stdin| {
            seen_clone.lock().unwrap().push(args.to_vec());
            let joined = args.join(" ");
            let stdout = if joined.contains("config view") {
                KUBECONFIG_JSON.to_owned()
            } else if joined.contains("get namespaces --no-headers") {
                "adx-mon Active 30d\ndayan Active 30d\nproduction Active 30d\n".to_owned()
            } else if joined.contains("/api/v1/pods?limit=200") {
                r#"{"kind":"PodList","metadata":{"resourceVersion":"9500","continue":"p2-tok","remainingItemCount":29999},"items":[{"metadata":{"name":"ingestor-0","namespace":"adx-mon","creationTimestamp":"2026-11-12T10:00:00Z"},"status":{"phase":"Running"}}]}"#.to_owned()
            } else if joined.contains("get pods --all-namespaces -o wide --no-headers") {
                let mut lines = String::with_capacity(30_000 * 80);
                for i in 0..29_999 {
                    lines.push_str(&format!(
                        "adx-mon ingestor-{i} 1/1 Running 0 5d 10.244.1.10 aks-pool-01 <none> <none>\n"
                    ));
                }
                // Place target pod at index 29,999 (past the 25,000 batch-parse guard!)
                lines.push_str(
                    "dayan brix-runner-29999 2/2 Running 1 12m 10.244.99.42 aks-gpu-77 <none> <none>\n",
                );
                lines
            } else if joined.contains("-n dayan get pods -o wide --no-headers") {
                "brix-runner-29999 2/2 Running 1 12m 10.244.99.42 aks-gpu-77 <none> <none>\nother-pod 1/1 Running 0 1d 10.244.99.10 aks-cpu-01 <none> <none>\n".to_owned()
            } else if joined.contains("get events --all-namespaces --no-headers") {
                "adx-mon 5m Normal Pulled pod/ingestor-0 Container image already present\ndayan 12s Warning BackOff pod/brix-runner-29999 Back-off restarting failed container brix\n".to_owned()
            } else if joined.contains("get configmaps --all-namespaces --no-headers") {
                return Err(openidea_plugin::Error::Failed(
                    "process stdout exceeded 16 MiB limit".into(),
                ));
            } else if joined.contains("/api/v1/configmaps?limit=250&continue=cm-tok-2") {
                r#"{"kind":"ConfigMapList","metadata":{"resourceVersion":"9602"},"items":[{"metadata":{"name":"brix-feature-flags","namespace":"dayan","creationTimestamp":"2026-11-12T10:00:00Z"},"data":{"ENABLE_GPU":"true"}}]}"#.to_owned()
            } else if joined.contains("/api/v1/configmaps?limit=250") {
                r#"{"kind":"ConfigMapList","metadata":{"resourceVersion":"9601","continue":"cm-tok-2","remainingItemCount":1},"items":[{"metadata":{"name":"adx-config","namespace":"adx-mon","creationTimestamp":"2026-11-12T10:00:00Z"},"data":{"A":"1"}}]}"#.to_owned()
            } else {
                String::new()
            };
            Ok(ProcessOutput {
                status: Some(0),
                stdout,
                stderr: String::new(),
            })
        });
        host.add_file("k8s/app.yaml", SAMPLE_MANIFEST);

        let mut plugin = KubernetesPlugin::default();
        plugin
            .activate(&host.context("dengliu.kubernetes"))
            .unwrap();

        // Load All-Namespaces Pods page 1 (only 1 pod loaded out of 30,000)
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"set-all-namespaces","all":true,"category":"Pod","limit":200}"#.into(),
                ),
                &host.invocation(),
            )
            .unwrap();
        assert_eq!(
            plugin
                .snapshot
                .as_ref()
                .unwrap()
                .resources
                .iter()
                .filter(|r| r.kind == "Pod")
                .count(),
            1
        );

        // 1. Ground-truth cluster search scans all 30,000 pods, finds pod #29,999, and leaves base snapshot intact
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"search-category","requestId":41,"category":"Pod","query":"dayan brix"}"#
                        .into(),
                ),
                &host.invocation(),
            )
            .unwrap();
        let search_msg = host
            .view_messages()
            .into_iter()
            .rev()
            .find_map(|(v, m)| {
                (v == VIEW_ID && m.contains(r#""op":"search-results""#)).then_some(m)
            })
            .expect("search-results message");
        let parsed_msg: serde_json::Value = serde_json::from_str(&search_msg).unwrap();
        assert_eq!(parsed_msg["requestId"], 41);
        assert_eq!(parsed_msg["scannedTotal"], 30_000);
        assert_eq!(parsed_msg["totalMatches"], 1);
        assert_eq!(parsed_msg["resources"][0]["name"], "brix-runner-29999");
        assert_eq!(parsed_msg["resources"][0]["namespace"], "dayan");
        assert_eq!(parsed_msg["resources"][0]["node_name"], "aks-gpu-77");
        // Base snapshot still holds only its original page-1 pod (`ingestor-0`)!
        let base_pods: Vec<&str> = plugin
            .snapshot
            .as_ref()
            .unwrap()
            .resources
            .iter()
            .filter(|r| r.kind == "Pod")
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(base_pods, vec!["ingestor-0"]);

        // 2. `dayan/brix` fast-path narrows directly to `-n dayan` instead of `--all-namespaces`
        seen_args.lock().unwrap().clear();
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"search-category","requestId":42,"category":"Pod","query":"dayan/brix"}"#
                        .into(),
                ),
                &host.invocation(),
            )
            .unwrap();
        let fast_calls = seen_args.lock().unwrap().clone();
        assert!(
            fast_calls
                .iter()
                .any(|a| a.windows(2).any(|w| w == ["-n", "dayan"])),
            "expected single-namespace fast path for dayan/brix, got {fast_calls:?}"
        );
        assert!(
            !fast_calls
                .iter()
                .any(|a| a.iter().any(|x| x == "--all-namespaces")),
            "must not scan --all-namespaces on dayan/brix fast path"
        );

        // 3. Ground-truth Event table search parses object kind/name + multi-word message
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"search-category","requestId":43,"category":"Event","query":"restarting failed"}"#
                        .into(),
                ),
                &host.invocation(),
            )
            .unwrap();
        let ev_msg: serde_json::Value = serde_json::from_str(
            &host
                .view_messages()
                .into_iter()
                .rev()
                .find_map(|(v, m)| (v == VIEW_ID && m.contains(r#""requestId":43"#)).then_some(m))
                .expect("event search-results"),
        )
        .unwrap();
        assert_eq!(ev_msg["scannedTotal"], 2);
        assert_eq!(ev_msg["totalMatches"], 1);
        assert_eq!(ev_msg["events"][0]["kind"], "Pod");
        assert_eq!(ev_msg["events"][0]["object"], "brix-runner-29999");

        // 4. 16 MiB stdout limit fallback walks `--raw ...?limit=500&continue=...` pages seamlessly
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"search-category","requestId":44,"category":"ConfigMap","query":"brix-feature"}"#
                        .into(),
                ),
                &host.invocation(),
            )
            .unwrap();
        let cm_msg: serde_json::Value = serde_json::from_str(
            &host
                .view_messages()
                .into_iter()
                .rev()
                .find_map(|(v, m)| (v == VIEW_ID && m.contains(r#""requestId":44"#)).then_some(m))
                .expect("configmap fallback search-results"),
        )
        .unwrap();
        assert_eq!(cm_msg["scannedTotal"], 2);
        assert_eq!(cm_msg["totalMatches"], 1);
        assert_eq!(cm_msg["resources"][0]["name"], "brix-feature-flags");
    }

    #[test]
    fn watch_cache_table_proxy_search_scans_gzipped_88k_pods_and_resolves_namespaces() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use openidea_plugin::HttpResponse;
        use std::io::Write as _;

        fn gzip_json(s: &str) -> Vec<u8> {
            let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
            enc.write_all(s.as_bytes()).unwrap();
            enc.finish().unwrap()
        }

        // Build an 88,049-row Kubernetes `Table` (`includeObject=None`) payload and gzip it (~1.5 MiB style)
        let mut all_pods_table = String::with_capacity(88_100 * 115);
        all_pods_table.push_str(
            r#"{"kind":"Table","apiVersion":"meta.k8s.io/v1","columnDefinitions":[{"name":"Name","priority":0},{"name":"Ready","priority":0},{"name":"Status","priority":0},{"name":"Restarts","priority":0},{"name":"Age","priority":0},{"name":"IP","priority":0},{"name":"Node","priority":0}],"rows":["#,
        );
        for i in 0..88_047 {
            if i > 0 {
                all_pods_table.push(',');
            }
            all_pods_table.push_str(&format!(
                r#"{{"cells":["worker-{i}","1/1","Running",0,"5d","10.20.{}.{}","aks-pool-01"],"object":null}}"#,
                (i / 250) % 250,
                (i % 250) + 1
            ));
        }
        // Row 88,047: pod inside `dengliu` namespace named `brix-git` (only matches `"deng"` via its namespace!)
        all_pods_table.push_str(
            r#",{"cells":["brix-git","1/1","Running",0,"14d","10.26.75.178","aks-gpu-01"],"object":null}"#,
        );
        // Row 88,048: pod in `shared-jobs` namespace named `deng-eval-job-9` (matches `"deng"` via pod name, needs Step 3 namespace resolution!)
        all_pods_table.push_str(
            r#",{"cells":["deng-eval-job-9","2/2","Running","1 (3m ago)","8m","10.26.99.9","aks-gpu-88"],"object":null}]}"#,
        );
        let gz_88k = gzip_json(&all_pods_table);
        assert!(
            gz_88k.len() < 3 * 1024 * 1024,
            "gzipped 88k Table stays well below 8 MiB fetch cap: {} bytes",
            gz_88k.len()
        );

        let host = install_mock_with_kubectl();
        host.on_process("kubectl", move |args, _stdin| {
            let joined = args.join(" ");
            if joined.contains("get pods --all-namespaces") {
                panic!("must use watch-cache Table proxy instead of slow 177-chunk kubectl get -A: {joined}");
            }
            let stdout = if joined.contains("config view") {
                KUBECONFIG_JSON.to_owned()
            } else if joined.contains("get namespaces --no-headers") {
                "adx-mon Active 30d\ndengliu Active 30d\nproduction Active 30d\nshared-jobs Active 30d\n".to_owned()
            } else if joined.contains("/api/v1/pods?limit=200") {
                r#"{"kind":"PodList","metadata":{"resourceVersion":"9500","continue":"p2-tok","remainingItemCount":88048},"items":[{"metadata":{"name":"worker-0","namespace":"adx-mon","creationTimestamp":"2026-11-12T10:00:00Z"},"status":{"phase":"Running"}}]}"#.to_owned()
            } else {
                String::new()
            };
            Ok(ProcessOutput {
                status: Some(0),
                stdout,
                stderr: String::new(),
            })
        });

        host.on_fetch(move |req| {
            assert!(
                req.headers
                    .iter()
                    .any(|(k, v)| k.eq_ignore_ascii_case("Accept") && v.contains("as=Table")),
                "expected Table Accept header: {:?}",
                req.headers
            );
            assert!(
                req.headers
                    .iter()
                    .any(|(k, v)| k.eq_ignore_ascii_case("Accept-Encoding") && v == "gzip"),
                "expected explicit gzip Accept-Encoding: {:?}",
                req.headers
            );
            let body = if req
                .url
                .contains("/api/v1/namespaces/dengliu/pods?resourceVersion=0&includeObject=Metadata")
            {
                br#"{"kind":"Table","apiVersion":"meta.k8s.io/v1","columnDefinitions":[{"name":"Name","priority":0}],"rows":[{"cells":["brix-git","1/1","Running",0,"14d","10.26.75.178","aks-gpu-01"],"object":{"metadata":{"name":"brix-git","namespace":"dengliu","ownerReferences":[{"kind":"StatefulSet","name":"brix"}]}}}]}"#.to_vec()
            } else if req
                .url
                .contains("/api/v1/pods?resourceVersion=0&includeObject=None")
            {
                gz_88k.clone()
            } else if req
                .url
                .contains("fieldSelector=metadata.name%3Ddeng-eval-job-9&includeObject=Metadata")
            {
                br#"{"kind":"Table","apiVersion":"meta.k8s.io/v1","columnDefinitions":[{"name":"Name","priority":0}],"rows":[{"cells":["deng-eval-job-9","2/2","Running","1 (3m ago)","8m","10.26.99.9","aks-gpu-88"],"object":{"metadata":{"name":"deng-eval-job-9","namespace":"shared-jobs","ownerReferences":[{"kind":"Job","name":"deng-eval"}]}}}]}"#.to_vec()
            } else if req
                .url
                .contains("/api/v1/secrets?resourceVersion=0&includeObject=Metadata")
            {
                gzip_json(
                    r#"{"kind":"Table","apiVersion":"meta.k8s.io/v1","columnDefinitions":[{"name":"Name","priority":0},{"name":"Type","priority":0},{"name":"Data","priority":0},{"name":"Age","priority":0}],"rows":[{"cells":["deng-tls-cert","kubernetes.io/tls",2,"10d"],"object":{"metadata":{"name":"deng-tls-cert","namespace":"dengliu"}}}]}"#,
                )
            } else {
                panic!("unexpected proxy URL: {}", req.url);
            };
            Ok(HttpResponse {
                status: 200,
                headers: vec![("content-type".into(), "application/json".into())],
                body,
            })
        });
        host.add_file("k8s/app.yaml", SAMPLE_MANIFEST);

        let mut plugin = KubernetesPlugin::default();
        plugin
            .activate(&host.context("dengliu.kubernetes"))
            .unwrap();

        // Load All-Namespaces Pods page 1 (1 loaded + 88,048 remaining = 88,049 > 40,000)
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"set-all-namespaces","all":true,"category":"Pod","limit":200}"#.into(),
                ),
                &host.invocation(),
            )
            .unwrap();

        // Install a live mock proxy child for `prod-east`
        let proxy_child = host::start(
            &host::Command::new("kubectl").args(["proxy", "--port=0"]),
            None,
        )
        .unwrap();
        plugin
            .watches
            .install_test_proxy("", "prod-east", 51466, proxy_child);

        // 1. Search `"deng"` across all 88,049 pods:
        //    - Step 1 finds `dengliu/brix-git` (matched via namespace `dengliu`!)
        //    - Step 2 streams all 88,049 gzipped pods and finds `deng-eval-job-9`
        //    - Step 3 resolves `shared-jobs` + `Job/deng-eval` via `fieldSelector=metadata.name=deng-eval-job-9`
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"search-category","requestId":91,"category":"Pod","query":"deng"}"#
                        .into(),
                ),
                &host.invocation(),
            )
            .unwrap();
        let pod_msg: serde_json::Value = serde_json::from_str(
            &host
                .view_messages()
                .into_iter()
                .rev()
                .find_map(|(v, m)| (v == VIEW_ID && m.contains(r#""requestId":91"#)).then_some(m))
                .expect("88k pod search-results"),
        )
        .unwrap();
        assert_eq!(pod_msg["scannedTotal"], 88_049);
        assert_eq!(pod_msg["totalMatches"], 2);
        assert_eq!(pod_msg["resources"][0]["namespace"], "dengliu");
        assert_eq!(pod_msg["resources"][0]["name"], "brix-git");
        assert_eq!(pod_msg["resources"][0]["controlled_by_kind"], "StatefulSet");
        assert_eq!(pod_msg["resources"][1]["namespace"], "shared-jobs");
        assert_eq!(pod_msg["resources"][1]["name"], "deng-eval-job-9");
        assert_eq!(pod_msg["resources"][1]["restarts"], 1);
        assert_eq!(pod_msg["resources"][1]["controlled_by_kind"], "Job");
        assert!(
            plugin.watches.api_proxy.is_none(),
            "ephemeral kubectl proxy must be stopped right after search-category finishes"
        );

        // 2. Search `"deng"` on Secrets (<= 12,000 items -> single `includeObject=Metadata` request, numeric Data cell `2`)
        let proxy_child_2 = host::start(
            &host::Command::new("kubectl").args(["proxy", "--port=0"]),
            None,
        )
        .unwrap();
        plugin
            .watches
            .install_test_proxy("", "prod-east", 51466, proxy_child_2);
        plugin
            .snapshot
            .as_mut()
            .unwrap()
            .overview_counts
            .insert("Secret".into(), 6_247);
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"search-category","requestId":92,"category":"Secret","query":"tls"}"#
                        .into(),
                ),
                &host.invocation(),
            )
            .unwrap();
        let sec_msg: serde_json::Value = serde_json::from_str(
            &host
                .view_messages()
                .into_iter()
                .rev()
                .find_map(|(v, m)| (v == VIEW_ID && m.contains(r#""requestId":92"#)).then_some(m))
                .expect("secret search-results"),
        )
        .unwrap();
        assert_eq!(sec_msg["scannedTotal"], 1);
        assert_eq!(sec_msg["totalMatches"], 1);
        assert_eq!(sec_msg["resources"][0]["name"], "deng-tls-cert");
        assert_eq!(sec_msg["resources"][0]["namespace"], "dengliu");
        assert_eq!(sec_msg["resources"][0]["ready"], "2 keys");
    }

    #[test]
    fn proxy_search_pages_table_on_response_size_cap_and_drops_stale_queued_scopes() {
        use openidea_plugin::HttpResponse;

        let host = install_mock_with_kubectl();
        host.on_process("kubectl", move |args, _stdin| {
            let joined = args.join(" ");
            if joined.contains("--raw /api/v1/pods?limit=250")
                || joined.contains("--raw /api/v1/pods?limit=500")
            {
                panic!("must page Table via proxy instead of raw full-JSON fallback: {joined}");
            }
            let stdout = if joined.contains("config view") {
                KUBECONFIG_JSON.to_owned()
            } else if joined.contains("get namespaces --no-headers") {
                "adx-mon Active 30d\ndengliu Active 30d\nshared-jobs Active 30d\n".to_owned()
            } else if joined.contains("/api/v1/pods?limit=200") {
                r#"{"kind":"PodList","metadata":{"resourceVersion":"9500","continue":"p2-tok","remainingItemCount":88048},"items":[{"metadata":{"name":"worker-0","namespace":"adx-mon","creationTimestamp":"2026-11-12T10:00:00Z"},"status":{"phase":"Running"}}]}"#.to_owned()
            } else if joined.contains("/api/v1/namespaces/dengliu/pods?limit=200") {
                r#"{"kind":"PodList","metadata":{"resourceVersion":"9505"},"items":[]}"#.to_owned()
            } else {
                String::new()
            };
            Ok(ProcessOutput {
                status: Some(0),
                stdout,
                stderr: String::new(),
            })
        });

        host.on_fetch(move |req| {
            if req
                .url
                .contains("/api/v1/pods?resourceVersion=0&includeObject=None")
            {
                // Simulate decompressed 88k-pod Table exceeding an 8 MiB `MAX_RESPONSE` host cap
                return Err(Error::Failed("response larger than 8 MiB".into()));
            }
            let body = if req
                .url
                .contains("/api/v1/pods?limit=20000&includeObject=None&continue=tbl-p2")
            {
                br#"{"kind":"Table","apiVersion":"meta.k8s.io/v1","metadata":{},"columnDefinitions":[{"name":"Name","priority":0}],"rows":[{"cells":["gpu-trainer-99","1/1","Running",0,"2h","10.20.9.9","aks-gpu-99"],"object":null}]}"#.to_vec()
            } else if req
                .url
                .contains("/api/v1/pods?limit=20000&includeObject=None")
            {
                br#"{"kind":"Table","apiVersion":"meta.k8s.io/v1","metadata":{"continue":"tbl-p2"},"columnDefinitions":[{"name":"Name","priority":0}],"rows":[{"cells":["worker-0","1/1","Running",0,"5d","10.20.0.1","aks-pool-01"],"object":null}]}"#.to_vec()
            } else if req
                .url
                .contains("fieldSelector=metadata.name%3Dgpu-trainer-99&includeObject=Metadata")
            {
                br#"{"kind":"Table","apiVersion":"meta.k8s.io/v1","columnDefinitions":[{"name":"Name","priority":0}],"rows":[{"cells":["gpu-trainer-99","1/1","Running",0,"2h","10.20.9.9","aks-gpu-99"],"object":{"metadata":{"namespace":"shared-jobs","ownerReferences":[{"kind":"Job","name":"gpu-trainer"}]}}}]}"#.to_vec()
            } else {
                panic!("unexpected proxy URL: {}", req.url);
            };
            Ok(HttpResponse {
                status: 200,
                headers: vec![("content-type".into(), "application/json".into())],
                body,
            })
        });
        host.add_file("k8s/app.yaml", SAMPLE_MANIFEST);

        let mut plugin = KubernetesPlugin::default();
        plugin
            .activate(&host.context("dengliu.kubernetes"))
            .unwrap();

        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"set-all-namespaces","all":true,"category":"Pod","limit":200}"#.into(),
                ),
                &host.invocation(),
            )
            .unwrap();

        let proxy_child = host::start(
            &host::Command::new("kubectl").args(["proxy", "--port=0"]),
            None,
        )
        .unwrap();
        plugin
            .watches
            .install_test_proxy("", "prod-east", 51466, proxy_child);

        // 1. Single-shot `resourceVersion=0&includeObject=None` hits `"response larger than 8 MiB"` ->
        //    automatically pages `?limit=20000&includeObject=None&continue=...` and resolves `shared-jobs`!
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"search-category","requestId":101,"category":"Pod","query":"gpu-trainer","context":"prod-east","scopeSignature":"*"}"#
                        .into(),
                ),
                &host.invocation(),
            )
            .unwrap();
        let msg: serde_json::Value = serde_json::from_str(
            &host
                .view_messages()
                .into_iter()
                .rev()
                .find_map(|(v, m)| (v == VIEW_ID && m.contains(r#""requestId":101"#)).then_some(m))
                .expect("paged proxy search-results"),
        )
        .unwrap();
        assert_eq!(msg["scannedTotal"], 2);
        assert_eq!(msg["totalMatches"], 1);
        assert_eq!(msg["resources"][0]["name"], "gpu-trainer-99");
        assert_eq!(msg["resources"][0]["namespace"], "shared-jobs");

        // 2. User switches to `dengliu` namespace -> any queued `scopeSignature: "*"` search is dropped in 0 HTTP requests!
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"set-namespace","namespace":"dengliu","category":"Pod","limit":200}"#
                        .into(),
                ),
                &host.invocation(),
            )
            .unwrap();
        let reqs_before = host.requests().len();
        plugin
            .handle(
                VIEW_ID,
                ViewEvent::Message(
                    r#"{"op":"search-category","requestId":102,"category":"Pod","query":"gpu-trainer","context":"prod-east","scopeSignature":"*"}"#
                        .into(),
                ),
                &host.invocation(),
            )
            .unwrap();
        assert_eq!(
            host.requests().len(),
            reqs_before,
            "stale queued search-category must be dropped without issuing HTTP calls"
        );
    }

    #[test]
    fn detects_helm_kustomize_and_kubernetes_files_and_folders_for_icons() {
        let host = install_mock_with_kubectl();

        // 1. Standalone Kubernetes & CNCF CRD manifests (open-source podinfo / Flux layout)
        host.add_file(
            "podinfo/deploy/webapp/frontend-deployment.yaml",
            "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: frontend\n",
        );
        host.add_file(
            "podinfo/deploy/webapp/frontend-service.yaml",
            "apiVersion: v1\nkind: Service\nmetadata:\n  name: frontend\n",
        );
        host.add_file(
            "podinfo/flux/gitrepository.yaml",
            "apiVersion: source.toolkit.fluxcd.io/v1\nkind: GitRepository\nmetadata:\n  name: podinfo\n",
        );
        host.add_file(
            "podinfo/flux/helmrelease.yaml",
            "apiVersion: helm.toolkit.fluxcd.io/v2\nkind: HelmRelease\nmetadata:\n  name: podinfo\n",
        );

        // 2. Helm chart (`charts/podinfo`): chart dir, Chart.yaml, Chart.lock, values.yaml, values-prod.yaml, templates/deployment.yaml, _helpers.tpl, NOTES.txt, .helmignore, crds/, README.md
        host.add_file(
            "podinfo/charts/podinfo/Chart.yaml",
            "apiVersion: v2\nname: podinfo\nversion: 6.7.0\n",
        );
        host.add_file(
            "podinfo/charts/podinfo/Chart.lock",
            "dependencies: []\ndigest: sha256:abc\n",
        );
        host.add_file(
            "podinfo/charts/podinfo/values.yaml",
            "replicaCount: 2\nimage:\n  tag: 6.7.0\n",
        );
        host.add_file(
            "podinfo/charts/podinfo/values-prod.yaml",
            "replicaCount: 5\n",
        );
        host.add_file(
            "podinfo/charts/podinfo/templates/deployment.yaml",
            "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: {{ include \"podinfo.fullname\" . }}\n",
        );
        host.add_file(
            "podinfo/charts/podinfo/templates/_helpers.tpl",
            "{{- define \"podinfo.fullname\" -}}podinfo{{- end -}}\n",
        );
        host.add_file(
            "podinfo/charts/podinfo/templates/NOTES.txt",
            "Installed {{ .Release.Name }}\n",
        );
        host.add_file("podinfo/charts/podinfo/.helmignore", ".DS_Store\n");
        host.add_file(
            "podinfo/charts/podinfo/crds/alertprovider-crd.yaml",
            "apiVersion: apiextensions.k8s.io/v1\nkind: CustomResourceDefinition\nmetadata:\n  name: providers.notification.toolkit.fluxcd.io\n",
        );
        host.add_file("podinfo/charts/podinfo/README.md", "# podinfo Helm chart\n");

        // 3. Kustomize base & overlay (`overlays/prod`): dir, kustomization.yaml, strategic merge patch, JSON6902 patch, component dir, helmCharts valuesFile, and standard `resources:` manifest
        host.add_file(
            "podinfo/kustomize/base/kustomization.yaml",
            "apiVersion: kustomize.config.k8s.io/v1beta1\nkind: Kustomization\nresources:\n  - deployment.yaml\n",
        );
        host.add_file(
            "podinfo/kustomize/base/deployment.yaml",
            "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: podinfo\n",
        );
        host.add_file(
            "podinfo/kustomize/overlays/prod/kustomization.yaml",
            "apiVersion: kustomize.config.k8s.io/v1beta1\nkind: Kustomization\nresources:\n  - ../../base\ncomponents:\n  - ../../components/tls\npatches:\n  - path: patch-replicas.yaml\npatchesJson6902:\n  - path: patch-hpa.yaml\nhelmCharts:\n  - name: podinfo\n    valuesFile: helm-values-prod.yaml\n",
        );
        host.add_file(
            "podinfo/kustomize/overlays/prod/patch-replicas.yaml",
            "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: podinfo\nspec:\n  replicas: 5\n",
        );
        host.add_file(
            "podinfo/kustomize/overlays/prod/patch-hpa.yaml",
            "- op: replace\n  path: /spec/maxReplicas\n  value: 10\n",
        );
        host.add_file(
            "podinfo/kustomize/overlays/prod/helm-values-prod.yaml",
            "replicaCount: 4\n",
        );
        host.add_file(
            "podinfo/kustomize/overlays/prod/transformer.yaml",
            "apiVersion: builtin\nkind: PrefixSuffixTransformer\nmetadata:\n  name: prod-prefix\n",
        );

        // 4. Non-Kubernetes YAML files that must NOT be given Kubernetes/Helm/Kustomize icons
        host.add_file(
            "docker-compose.yaml",
            "services:\n  redis:\n    image: redis:7\n",
        );
        host.add_file(
            ".github/workflows/ci.yaml",
            "name: CI\non: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n",
        );
        host.add_file(
            "catalog-info.yaml",
            "apiVersion: backstage.io/v1alpha1\nkind: Component\nmetadata:\n  name: podinfo\n",
        );
        host.add_file(
            "misc/ignored.yaml",
            "# nonk8s\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: local-only\n",
        );

        let mut plugin = KubernetesPlugin::default();
        plugin
            .activate(&host.context("dengliu.kubernetes"))
            .unwrap();

        let icons: std::collections::BTreeMap<String, String> = host
            .file_icons()
            .into_iter()
            .map(|((_, path), (icon, _))| (path, icon))
            .collect();
        let icon_of = |suffix: &str| -> Option<&str> {
            icons
                .iter()
                .find_map(|(k, v)| k.ends_with(suffix).then_some(v.as_str()))
        };

        // Helm chart directory & files
        assert_eq!(icon_of("/podinfo/charts/podinfo"), Some("helm-chart-dir"));
        assert_eq!(
            icon_of("/podinfo/charts/podinfo/Chart.yaml"),
            Some("helm-chart")
        );
        assert_eq!(
            icon_of("/podinfo/charts/podinfo/Chart.lock"),
            Some("helm-chart")
        );
        assert_eq!(
            icon_of("/podinfo/charts/podinfo/values.yaml"),
            Some("helm-values")
        );
        assert_eq!(
            icon_of("/podinfo/charts/podinfo/values-prod.yaml"),
            Some("helm-values")
        );
        assert_eq!(
            icon_of("/podinfo/charts/podinfo/templates/deployment.yaml"),
            Some("helm-template"),
            "Helm template with apiVersion/kind inside templates/ must get helm-template, not kubernetes"
        );
        assert_eq!(
            icon_of("/podinfo/charts/podinfo/templates/_helpers.tpl"),
            Some("helm-template")
        );
        assert_eq!(
            icon_of("/podinfo/charts/podinfo/templates/NOTES.txt"),
            Some("helm-template")
        );
        assert_eq!(
            icon_of("/podinfo/charts/podinfo/.helmignore"),
            Some("helm-template")
        );
        assert_eq!(
            icon_of("/podinfo/charts/podinfo/crds/alertprovider-crd.yaml"),
            Some("kubernetes"),
            "Plain un-templated CRD under crds/ keeps kubernetes icon"
        );
        assert_eq!(icon_of("/podinfo/charts/podinfo/README.md"), None);

        // Kustomize directories, root files, patches, plugins, and `resources:` manifests
        assert_eq!(icon_of("/podinfo/kustomize/base"), Some("kustomize-dir"));
        assert_eq!(
            icon_of("/podinfo/kustomize/overlays/prod"),
            Some("kustomize-dir")
        );
        assert_eq!(
            icon_of("/podinfo/kustomize/components/tls"),
            Some("kustomize-dir")
        );
        assert_eq!(
            icon_of("/podinfo/kustomize/base/kustomization.yaml"),
            Some("kustomize")
        );
        assert_eq!(
            icon_of("/podinfo/kustomize/overlays/prod/kustomization.yaml"),
            Some("kustomize")
        );
        assert_eq!(
            icon_of("/podinfo/kustomize/overlays/prod/patch-replicas.yaml"),
            Some("kustomize-patch"),
            "Strategic merge patch referenced in patches: must get kustomize-patch, not kubernetes"
        );
        assert_eq!(
            icon_of("/podinfo/kustomize/overlays/prod/patch-hpa.yaml"),
            Some("kustomize-patch")
        );
        assert_eq!(
            icon_of("/podinfo/kustomize/overlays/prod/transformer.yaml"),
            Some("kustomize-patch")
        );
        assert_eq!(
            icon_of("/podinfo/kustomize/overlays/prod/helm-values-prod.yaml"),
            Some("helm-values")
        );
        assert_eq!(
            icon_of("/podinfo/kustomize/base/deployment.yaml"),
            Some("kubernetes"),
            "Standard K8s manifest under resources: keeps kubernetes icon (IntelliJ default)"
        );

        // Standalone Kubernetes manifests
        assert_eq!(
            icon_of("/podinfo/deploy/webapp/frontend-deployment.yaml"),
            Some("kubernetes")
        );
        assert_eq!(
            icon_of("/podinfo/deploy/webapp/frontend-service.yaml"),
            Some("kubernetes")
        );
        assert_eq!(
            icon_of("/podinfo/flux/gitrepository.yaml"),
            Some("kubernetes")
        );
        assert_eq!(
            icon_of("/podinfo/flux/helmrelease.yaml"),
            Some("kubernetes")
        );

        // Non-Kubernetes YAML files stay untouched
        assert_eq!(icon_of("/docker-compose.yaml"), None);
        assert_eq!(icon_of("/.github/workflows/ci.yaml"), None);
        assert_eq!(icon_of("/catalog-info.yaml"), None);
        assert_eq!(icon_of("/misc/ignored.yaml"), None);

        // Live file-changed event updates icons when a new manifest is added
        host.add_file(
            "podinfo/flux/kustomization-sync.yaml",
            "apiVersion: kustomize.toolkit.fluxcd.io/v1\nkind: Kustomization\nmetadata:\n  name: podinfo-sync\n",
        );
        plugin.handle_events(
            &[Event::Other {
                kind: "files-changed".into(),
                data: r#"{"created":["podinfo/flux/kustomization-sync.yaml"],"modified":[],"removed":[]}"#.into(),
            }],
            &host.invocation(),
        );
        let updated_icons: std::collections::BTreeMap<String, String> = host
            .file_icons()
            .into_iter()
            .map(|((_, path), (icon, _))| (path, icon))
            .collect();
        assert_eq!(
            updated_icons.iter().find_map(|(k, v)| k
                .ends_with("/podinfo/flux/kustomization-sync.yaml")
                .then_some(v.as_str())),
            Some("kubernetes")
        );
    }

    #[test]
    fn plain_manifest_completions_and_links() {
        let host = MockHost::install();
        let deploy_yaml = "\
apiVersion: apps/v1
kind: Deployment
metadata:
  name: simple-app
  labels:
    app: simple-app
spec:
  replicas: 2
  selector:
    matchLabels:
      app: simple-app
  template:
    metadata:
      labels:
        app: simple-app
    spec:
      serviceAccountName: simple-app
      v
      volumes:
        - name: app-config
          configMap:
            name: simple-app-cfg
      containers:
        - name: simple-app
          image: nginx:1.27-alpine
          volumeMounts:
            - name: app-config
              mountPath: /etc/app
          resources:
            requests:
              c
";
        let hpa_yaml = "\
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata:
  name: simple-app
spec:
  scaleTargetRef:
    apiVersion: apps/v1
    kind: Deployment
    name: simple-app
  m
";
        host.add_file("plain-manifest/deployment.yaml", deploy_yaml);
        host.add_file("plain-manifest/hpa.yaml", hpa_yaml);
        host.add_file(
            "plain-manifest/secret.yaml",
            "apiVersion: v1\nkind: Secret\nmetadata:\n  name: simple-app-secret\ntype: Opaque\n",
        );

        let mut plugin = KubernetesPlugin::default();
        let deploy_doc = Document {
            path: "/project/plain-manifest/deployment.yaml".into(),
            language: "yaml".into(),
            version: 1,
            text: deploy_yaml.into(),
        };

        // 1. Typing `v` under `spec.template.spec` (line 17, col 7) suggests `volumes`
        let podspec_items = plugin.completion(&deploy_doc, Position::new(17, 7), None);
        let podspec_labels: Vec<&str> = podspec_items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            podspec_labels.contains(&"volumes"),
            "expected 'volumes' under spec.template.spec, got {podspec_labels:?}"
        );

        // 2. Typing `c` under `containers[].resources.requests` (line 30, col 15) suggests `cpu`
        let res_items = plugin.completion(&deploy_doc, Position::new(30, 15), None);
        let res_labels: Vec<&str> = res_items.iter().map(|i| i.label.as_str()).collect();
        assert!(res_labels.contains(&"cpu"), "got {res_labels:?}");

        // 3. `volumeMounts[].name: app-config` (line 26, col 22) jumps to `volumes[].name: app-config` (line 19)
        let vol_defs = plugin.definition(&deploy_doc, Position::new(26, 22));
        assert_eq!(vol_defs.len(), 1);
        assert_eq!(vol_defs[0].range.start.line, 19);

        // 4. HPA `spec:` typing `m` (line 9, col 3) suggests `minReplicas`, `maxReplicas`, `metrics` (not Deployment `template`)
        let hpa_doc = Document {
            path: "/project/plain-manifest/hpa.yaml".into(),
            language: "yaml".into(),
            version: 1,
            text: hpa_yaml.into(),
        };
        let hpa_spec_items = plugin.completion(&hpa_doc, Position::new(9, 3), None);
        let hpa_labels: Vec<&str> = hpa_spec_items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            hpa_labels.contains(&"minReplicas")
                && hpa_labels.contains(&"maxReplicas")
                && hpa_labels.contains(&"metrics"),
            "got {hpa_labels:?}"
        );

        // 5. HPA `scaleTargetRef.name: simple-app` (line 8, col 12) jumps to `Deployment/simple-app` (line 3) and completes workload name
        let hpa_defs = plugin.definition(&hpa_doc, Position::new(8, 12));
        assert_eq!(hpa_defs.len(), 1);
        assert!(hpa_defs[0].path.ends_with("plain-manifest/deployment.yaml"));
        assert_eq!(hpa_defs[0].range.start.line, 3);

        let hpa_name_edit = Document {
            text: hpa_yaml.replace("name: simple-app\n  m", "name: sim\n"),
            ..hpa_doc
        };
        let target_items = plugin.completion(&hpa_name_edit, Position::new(8, 13), None);
        let target_labels: Vec<&str> = target_items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            target_labels.contains(&"simple-app"),
            "got {target_labels:?}"
        );
    }

    #[test]
    fn simple_app_chart_completions_and_links() {
        let host = MockHost::install();
        host.add_file(
            "simple-app-chart/Chart.yaml",
            "apiVersion: v2\nname: simple-app-chart\ntype: application\nversion: 0.1.0\nappVersion: \"1.0.0\"\nd\n",
        );
        host.add_file(
            "simple-app-chart/values.yaml",
            "replicaCount: 2\nimage:\n  repository: nginx\n  tag: \"1.27-alpine\"\napps:\n  - name: frontend\n    namespace: prod\nconfig:\n  spec:\n    server: https://kubernetes.default.svc\n",
        );
        // Unrelated second chart in workspace to verify chart-scoped isolation
        host.add_file(
            "other-chart/Chart.yaml",
            "apiVersion: v2\nname: other\nversion: 0.1.0\n",
        );
        host.add_file("other-chart/values.yaml", "leakedOtherChartKey: true\n");

        host.add_file(
            "simple-app-chart/templates/_helpers.tpl",
            "{{- define \"simple-app-chart.fullname\" -}}\n{{- .Chart.Name }}\n{{- end }}\n",
        );
        let tpl = "\
{{- range .Values.apps }}
{{- $cfg := $.Values.config -}}
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ include \"simple-app-chart.fullname\" . }}-{{ .name }}
  namespace: {{ .namespace }}
spec:
  replicas: {{ $.Values.replicaCount }}
  template:
    spec:
      v
      containers:
        - name: {{ $.Chart.Name }}
          image: \"{{ $.Values.image.repository }}:{{ $cfg.spec.server }}\"
{{- end }}
";
        host.add_file("simple-app-chart/templates/deployment.yaml", tpl);

        let mut plugin = KubernetesPlugin::default();

        // 1. `Chart.yaml` completes `description` / `dependencies` at `d`, without Kubernetes `data` or `kdep`
        let chart_doc = Document {
            path: "/project/simple-app-chart/Chart.yaml".into(),
            language: "yaml".into(),
            version: 1,
            text: "apiVersion: v2\nname: simple-app-chart\nd\n".into(),
        };
        let chart_items = plugin.completion(&chart_doc, Position::new(2, 1), None);
        let chart_labels: Vec<&str> = chart_items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            chart_labels.contains(&"description") && chart_labels.contains(&"dependencies"),
            "got {chart_labels:?}"
        );
        assert!(
            !chart_labels.contains(&"data"),
            "Chart.yaml must not suggest ConfigMap `data`"
        );

        // 2. `templates/deployment.yaml` completes `volumes` under `spec.template.spec` (line 11, col 7) even with `{{- $cfg := ... -}}`
        let tpl_doc = Document {
            path: "/project/simple-app-chart/templates/deployment.yaml".into(),
            language: "helm-template".into(),
            version: 1,
            text: tpl.into(),
        };
        let podspec_items = plugin.completion(&tpl_doc, Position::new(11, 7), None);
        let podspec_labels: Vec<&str> = podspec_items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            podspec_labels.contains(&"volumes"),
            "expected 'volumes' in Helm template PodSpec, got {podspec_labels:?}"
        );

        // 3. Scoped `.name` inside `{{- range .Values.apps }}` (line 5, col 58) jumps to `apps.name` in `simple-app-chart/values.yaml` (line 5)
        let scoped_defs = plugin.definition(&tpl_doc, Position::new(5, 58));
        assert_eq!(scoped_defs.len(), 1, "expected scoped .name definition");
        assert!(
            scoped_defs[0]
                .path
                .ends_with("simple-app-chart/values.yaml")
        );
        assert_eq!(scoped_defs[0].range.start.line, 5);

        // 4. `$cfg` (line 14, col 55) jumps to `{{- $cfg := $.Values.config -}}` (line 1), and `.spec.server` (col 63) jumps to `values.yaml` (line 9)
        let var_defs = plugin.definition(&tpl_doc, Position::new(14, 55));
        assert_eq!(var_defs.len(), 1);
        assert!(var_defs[0].path.ends_with("templates/deployment.yaml"));
        assert_eq!(var_defs[0].range.start.line, 1);

        let var_field_defs = plugin.definition(&tpl_doc, Position::new(14, 63));
        assert_eq!(var_field_defs.len(), 1);
        assert!(
            var_field_defs[0]
                .path
                .ends_with("simple-app-chart/values.yaml")
        );
        assert_eq!(var_field_defs[0].range.start.line, 9);

        // 5. Segment-aware `.Values.image.` completion returns `repository` & `tag`, and `.Values.` excludes `other-chart` keys
        let dot_edit = Document {
            text: "replicas: {{ .Values.image. }}\n".into(),
            ..tpl_doc.clone()
        };
        let img_items = plugin.completion(&dot_edit, Position::new(0, 27), Some("."));
        let img_labels: Vec<&str> = img_items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            img_labels.contains(&"repository") && img_labels.contains(&"tag"),
            "got {img_labels:?}"
        );

        let root_vals_edit = Document {
            text: "replicas: {{ .Values. }}\n".into(),
            ..tpl_doc.clone()
        };
        let root_val_items = plugin.completion(&root_vals_edit, Position::new(0, 21), Some("."));
        let root_val_labels: Vec<&str> = root_val_items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            root_val_labels.contains(&"replicaCount")
                && !root_val_labels.contains(&"leakedOtherChartKey"),
            "got {root_val_labels:?}"
        );

        // 6. `.Chart.` & `$cfg.spec.` completions inside `{{ ... }}`
        let chart_obj_edit = Document {
            text: "name: {{ .Chart. }}\n".into(),
            ..tpl_doc.clone()
        };
        let chart_obj_items = plugin.completion(&chart_obj_edit, Position::new(0, 16), Some("."));
        let chart_obj_labels: Vec<&str> =
            chart_obj_items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            chart_obj_labels.contains(&"Name") && chart_obj_labels.contains(&"AppVersion"),
            "got {chart_obj_labels:?}"
        );

        // 7. Reverse navigation from `values.yaml` `replicaCount` (line 0) jumps to `templates/deployment.yaml` (line 8)
        let _ = plugin.line_markers(&tpl_doc);
        let values_doc = Document {
            path: "/project/simple-app-chart/values.yaml".into(),
            language: "yaml".into(),
            version: 1,
            text: "replicaCount: 2\nimage:\n  repository: nginx\n  tag: \"1.27-alpine\"\n".into(),
        };
        let rev_defs = plugin.definition(&values_doc, Position::new(0, 4));
        assert!(
            rev_defs
                .iter()
                .any(|d| d.path.ends_with("templates/deployment.yaml") && d.range.start.line == 8),
            "expected reverse link from values.yaml to templates/deployment.yaml, got {rev_defs:?}"
        );
    }

    #[test]
    fn simple_app_kustomize_completions_links_and_patch_inspections() {
        let host = MockHost::install();
        host.add_file(
            "simple-app-kustomize/base/kustomization.yaml",
            "apiVersion: kustomize.config.k8s.io/v1beta1\nkind: Kustomization\nresources:\n  - deployment.yaml\n",
        );
        host.add_file(
            "simple-app-kustomize/base/deployment.yaml",
            "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: simple-app\nspec:\n  replicas: 2\n  selector:\n    matchLabels:\n      app: simple-app\n  template:\n    metadata:\n      labels:\n        app: simple-app\n    spec:\n      containers:\n        - name: simple-app\n          image: nginx:1.27-alpine\n          resources:\n            requests:\n              cpu: 100m\n              memory: 128Mi\n            limits:\n              cpu: 500m\n              memory: 256Mi\n",
        );
        let prod_kust = "\
apiVersion: kustomize.config.k8s.io/v1beta1
kind: Kustomization
namespace: prod
resources:
  - ../../base
patches:
  - path: patch-deployment.yaml
  - path: patch-json6902.yaml
    target:
      kind: Deployment
      name: s
images:
  - name: n
";
        let patch_deploy = "\
apiVersion: apps/v1
kind: Deployment
metadata:
  name: simple-app
spec:
  replicas: 3
  template:
    spec:
      containers:
        - name: simple-app
          env:
            - name: LOG_LEVEL
              value: debug
";
        let patch_json = "\
- op: replace
  path: /spec/replicas
  value: 5
- o
";
        host.add_file(
            "simple-app-kustomize/overlays/prod/kustomization.yaml",
            prod_kust,
        );
        host.add_file(
            "simple-app-kustomize/overlays/prod/patch-deployment.yaml",
            patch_deploy,
        );
        host.add_file(
            "simple-app-kustomize/overlays/prod/patch-json6902.yaml",
            patch_json,
        );

        let mut plugin = KubernetesPlugin::default();
        plugin
            .activate(&host.context("dengliu.kubernetes"))
            .unwrap();

        // 1. `resources: - ../../base` (line 4, col 8) jumps directly to `base/kustomization.yaml`!
        let kust_doc = Document {
            path: "/project/simple-app-kustomize/overlays/prod/kustomization.yaml".into(),
            language: "yaml".into(),
            version: 1,
            text: prod_kust.into(),
        };
        let base_defs = plugin.definition(&kust_doc, Position::new(4, 8));
        assert_eq!(base_defs.len(), 1, "expected ../../base directory link");
        assert!(
            base_defs[0]
                .path
                .ends_with("simple-app-kustomize/base/kustomization.yaml"),
            "got {:?}",
            base_defs[0]
        );

        // 2. `kustomization.yaml` completes target workload `simple-app` (line 10, col 13) & image `nginx` (line 12, col 11)
        let target_name_items = plugin.completion(&kust_doc, Position::new(10, 13), None);
        assert!(
            target_name_items.iter().any(|i| i.label == "simple-app"),
            "got {target_name_items:?}"
        );
        let img_items = plugin.completion(&kust_doc, Position::new(12, 11), None);
        assert!(
            img_items.iter().any(|i| i.label == "nginx"),
            "got {img_items:?}"
        );

        // 3. `kustomization.yaml` relative path completion suggests `../../base` under `resources:`
        let path_edit_doc = Document {
            text: "apiVersion: kustomize.config.k8s.io/v1beta1\nkind: Kustomization\nresources:\n  - ../\n".into(),
            ..kust_doc.clone()
        };
        let rel_items = plugin.completion(&path_edit_doc, Position::new(3, 7), None);
        assert!(
            rel_items.iter().any(|i| i.label == "../../base"),
            "got {rel_items:?}"
        );
        // Restore full `kustomization.yaml` in index before validating patch-aware inspections
        let _ = plugin.line_markers(&kust_doc);

        // 4. Strategic merge `patch-deployment.yaml` completes `volumes` under `spec.template.spec` (line 8, col 7)
        //    AND does NOT emit false `missing-resource-limits` warnings on partial container overrides!
        let patch_edit_doc = Document {
            path: "/project/simple-app-kustomize/overlays/prod/patch-deployment.yaml".into(),
            language: "yaml".into(),
            version: 1,
            text: "\
apiVersion: apps/v1
kind: Deployment
metadata:
  name: simple-app
spec:
  replicas: 3
  template:
    spec:
      v
      containers:
        - name: simple-app
          env:
            - name: LOG_LEVEL
              value: debug
"
            .into(),
        };
        let patch_podspec = plugin.completion(&patch_edit_doc, Position::new(8, 7), None);
        assert!(
            patch_podspec.iter().any(|i| i.label == "volumes"),
            "got {patch_podspec:?}"
        );
        let diag_count = plugin
            .check_and_publish(
                "/project/simple-app-kustomize/overlays/prod/patch-deployment.yaml",
                false,
            )
            .unwrap();
        assert_eq!(
            diag_count, 0,
            "strategic merge patch inheriting resources from base should emit 0 false warnings"
        );

        // 5. JSON 6902 `patch-json6902.yaml` completes `op` on `- o` (line 3, col 3) and enum values on `op: `
        let json_patch_doc = Document {
            path: "/project/simple-app-kustomize/overlays/prod/patch-json6902.yaml".into(),
            language: "yaml".into(),
            version: 1,
            text: patch_json.into(),
        };
        let op_key_items = plugin.completion(&json_patch_doc, Position::new(3, 3), None);
        assert!(
            op_key_items.iter().any(|i| i.label == "op"),
            "got {op_key_items:?}"
        );
        let op_val_doc = Document {
            text: "- op: rep\n".into(),
            ..json_patch_doc
        };
        let op_val_items = plugin.completion(&op_val_doc, Position::new(0, 9), None);
        assert!(
            op_val_items.iter().any(|i| i.label == "replace"),
            "got {op_val_items:?}"
        );
    }

    #[test]
    fn multi_project_activation_keeps_navigation_completions_and_icons_project_scoped() {
        let host = MockHost::install();
        host.set_project_root(Some("/project-a"));
        host.add_project("/project-b");

        let shared_sa = "apiVersion: v1\nkind: ServiceAccount\nmetadata:\n  name: shared\n";
        let b_only_sa = "apiVersion: v1\nkind: ServiceAccount\nmetadata:\n  name: b-only-sa\n";
        let b_only_secret = "apiVersion: v1\nkind: Secret\nmetadata:\n  name: b-only-secret\nstringData:\n  TOKEN: abc\n";
        let pod_a = "\
apiVersion: v1
kind: Pod
metadata:
  name: pod-a
spec:
  serviceAccountName: shared
  containers:
    - name: app
      image: nginx:1.27-alpine
      envFrom:
        - secretRef:
            name: b-only-secret
      resources:
        requests:
          cpu: 100m
          memory: 128Mi
        limits:
          cpu: 250m
          memory: 256Mi
";
        let pod_b = "\
apiVersion: v1
kind: Pod
metadata:
  name: pod-b
spec:
  serviceAccountName: shared
  containers:
    - name: app
      image: nginx:1.27-alpine
";

        host.add_file("/project-a/account.yaml", shared_sa);
        host.add_file("/project-a/pod.yaml", pod_a);
        host.add_file("/project-b/account.yaml", shared_sa);
        host.add_file("/project-b/extra-sa.yaml", b_only_sa);
        host.add_file("/project-b/secret.yaml", b_only_secret);
        host.add_file("/project-b/pod.yaml", pod_b);

        let mut plugin = KubernetesPlugin::default();
        plugin
            .activate(&host.context("dengliu.kubernetes"))
            .unwrap();

        // 1. Both open projects were warmed into isolated indices during `activate()`,
        //    and icons publish cleanly for whichever window is active.
        assert!(plugin.indices.contains_key("/project-a"));
        assert!(plugin.indices.contains_key("/project-b"));
        let icons_a = host.file_icons();
        assert!(
            icons_a
                .iter()
                .any(|((_, p), (ic, _))| p == "/project-a/account.yaml" && ic == "kubernetes"),
            "expected icon for /project-a/account.yaml, got {icons_a:?}"
        );
        host.set_project_root(Some("/project-b"));
        plugin.sync_all_project_icons();
        let icons_b = host.file_icons();
        assert!(
            icons_b
                .iter()
                .any(|((_, p), (ic, _))| p == "/project-b/account.yaml" && ic == "kubernetes"),
            "expected icon for /project-b/account.yaml, got {icons_b:?}"
        );
        host.set_project_root(Some("/project-a"));

        // 2. Go to Definition on `serviceAccountName: shared` (line 5, col 24) in `/project-a/pod.yaml`
        //    must return ONLY `/project-a/account.yaml`, never `/project-b/account.yaml`.
        let pod_a_doc = Document {
            path: "/project-a/pod.yaml".into(),
            language: "yaml".into(),
            version: 1,
            text: pod_a.into(),
        };
        let defs_a: Vec<String> = plugin
            .definition(&pod_a_doc, Position::new(5, 24))
            .into_iter()
            .map(|d| d.path)
            .collect();
        assert_eq!(defs_a, vec!["/project-a/account.yaml".to_owned()]);

        // 3. Go to Definition on `serviceAccountName: shared` in `/project-b/pod.yaml`
        //    must return ONLY `/project-b/account.yaml`.
        let pod_b_doc = Document {
            path: "/project-b/pod.yaml".into(),
            language: "yaml".into(),
            version: 1,
            text: pod_b.into(),
        };
        let defs_b: Vec<String> = plugin
            .definition(&pod_b_doc, Position::new(5, 24))
            .into_iter()
            .map(|d| d.path)
            .collect();
        assert_eq!(defs_b, vec!["/project-b/account.yaml".to_owned()]);

        // 4. Completions and gutter markers in `/project-a/pod.yaml` never leak resources from `/project-b`
        let sa_edit_a = Document {
            text: "apiVersion: v1\nkind: Pod\nmetadata:\n  name: pod-a\nspec:\n  serviceAccountName: \n".into(),
            ..pod_a_doc.clone()
        };
        let sa_items_a: Vec<String> = plugin
            .completion(&sa_edit_a, Position::new(5, 22), None)
            .into_iter()
            .map(|i| i.label)
            .collect();
        assert!(sa_items_a.contains(&"shared".to_owned()));
        assert!(
            !sa_items_a.contains(&"b-only-sa".to_owned()),
            "leaked ServiceAccount from /project-b into /project-a completions: {sa_items_a:?}"
        );

        let markers_a = plugin.line_markers(&pod_a_doc);
        assert!(
            !markers_a
                .iter()
                .any(|m| m.tooltip.contains("b-only-secret")),
            "leaked Secret marker from /project-b into /project-a: {markers_a:?}"
        );
    }

    #[test]
    fn helm_lexical_variable_scopes_bind_per_reference_and_restore_after_shadowing() {
        let host = MockHost::install();
        host.add_file(
            "shadow-chart/Chart.yaml",
            "apiVersion: v2\nname: shadow-chart\nversion: 0.1.0\n",
        );
        host.add_file(
            "shadow-chart/values.yaml",
            "outer:\n  name: OUTER\n  outer_only: true\ninner:\n  name: INNER\n  inner_only: true\n",
        );
        let tpl = "\
{{- $cfg := .Values.outer -}}
before: {{ $cfg.name }}
{{- with .Values.inner -}}
{{- $cfg := . -}}
inside: {{ $cfg.name }}
{{- end -}}
after: {{ $cfg.name }}
";
        host.add_file("shadow-chart/templates/configmap.yaml", tpl);

        let mut plugin = KubernetesPlugin::default();
        plugin
            .activate(&host.context("dengliu.kubernetes"))
            .unwrap();

        let doc = Document {
            path: "/project/shadow-chart/templates/configmap.yaml".into(),
            language: "helm-template".into(),
            version: 1,
            text: tpl.into(),
        };

        // 1. `before: {{ $cfg.name }}` (line 1): `$cfg` -> line 0, `.name` -> `outer.name` (line 1 of values.yaml)
        let before_var = plugin.definition(&doc, Position::new(1, 12));
        assert_eq!(before_var.len(), 1);
        assert_eq!(before_var[0].path, doc.path);
        assert_eq!(before_var[0].range.start.line, 0);

        let before_prop = plugin.definition(&doc, Position::new(1, 17));
        assert_eq!(before_prop.len(), 1);
        assert!(before_prop[0].path.ends_with("shadow-chart/values.yaml"));
        assert_eq!(before_prop[0].range.start.line, 1);

        // 2. `inside: {{ $cfg.name }}` (line 4): `$cfg` -> line 3, `.name` -> `inner.name` (line 4 of values.yaml)
        let inside_var = plugin.definition(&doc, Position::new(4, 12));
        assert_eq!(inside_var.len(), 1);
        assert_eq!(inside_var[0].path, doc.path);
        assert_eq!(inside_var[0].range.start.line, 3);

        let inside_prop = plugin.definition(&doc, Position::new(4, 17));
        assert_eq!(inside_prop.len(), 1);
        assert!(inside_prop[0].path.ends_with("shadow-chart/values.yaml"));
        assert_eq!(inside_prop[0].range.start.line, 4);

        // 3. `after: {{ $cfg.name }}` (line 6): `$cfg` -> line 0 (restored!), `.name` -> `outer.name` (line 1)
        let after_var = plugin.definition(&doc, Position::new(6, 11));
        assert_eq!(after_var.len(), 1);
        assert_eq!(after_var[0].path, doc.path);
        assert_eq!(after_var[0].range.start.line, 0);

        let after_prop = plugin.definition(&doc, Position::new(6, 16));
        assert_eq!(after_prop.len(), 1);
        assert!(after_prop[0].path.ends_with("shadow-chart/values.yaml"));
        assert_eq!(after_prop[0].range.start.line, 1);

        // 4. Completion on `$cfg.` inside `with` offers `inner_only` and omits `outer_only`
        let inside_comp_doc = Document {
            text: "\
{{- $cfg := .Values.outer -}}
{{- with .Values.inner -}}
{{- $cfg := . -}}
inside: {{ $cfg.
{{- end -}}
"
            .into(),
            ..doc.clone()
        };
        let inside_labels: Vec<String> = plugin
            .completion(&inside_comp_doc, Position::new(3, 16), Some("."))
            .into_iter()
            .map(|i| i.label)
            .collect();
        assert!(
            inside_labels.contains(&"inner_only".to_owned())
                && !inside_labels.contains(&"outer_only".to_owned()),
            "got {inside_labels:?}"
        );

        // 5. Completion on `$cfg.` after `end` offers `outer_only` and omits `inner_only`,
        //    and range-header `$idx` goes out of scope after `end`
        let after_comp_doc = Document {
            text: "\
{{- $cfg := .Values.outer -}}
{{- range $idx, $cfg := .Values.inner -}}
inside: {{ $cfg.name }}
{{- end -}}
after: {{ $cfg.
"
            .into(),
            ..doc.clone()
        };
        let after_labels: Vec<String> = plugin
            .completion(&after_comp_doc, Position::new(4, 15), Some("."))
            .into_iter()
            .map(|i| i.label)
            .collect();
        assert!(
            after_labels.contains(&"outer_only".to_owned())
                && !after_labels.contains(&"inner_only".to_owned()),
            "got {after_labels:?}"
        );

        let after_vars_doc = Document {
            text: "\
{{- $cfg := .Values.outer -}}
{{- range $idx, $cfg := .Values.inner -}}
{{- end -}}
after: {{ $
"
            .into(),
            ..doc
        };
        let visible_vars: Vec<String> = plugin
            .completion(&after_vars_doc, Position::new(3, 11), Some("$"))
            .into_iter()
            .map(|i| i.label)
            .collect();
        assert!(visible_vars.contains(&"$cfg".to_owned()));
        assert!(
            !visible_vars.contains(&"$idx".to_owned()),
            "range-scoped $idx leaked past end: {visible_vars:?}"
        );
    }

    #[test]
    fn kustomize_image_name_completion_preserves_registry_host_and_port() {
        let host = MockHost::install();
        host.add_file(
            "kustomize-reg/deployment.yaml",
            "\
apiVersion: apps/v1
kind: Deployment
metadata:
  name: registry-app
spec:
  selector:
    matchLabels:
      app: registry-app
  template:
    metadata:
      labels:
        app: registry-app
    spec:
      containers:
        - name: web
          image: localhost:5000/team/app:v1
        - name: worker
          image: registry.corp.internal:8443/ns/worker@sha256:0123456789abcdef
        - name: ipv6
          image: \"[::1]:5000/org/svc:v2@sha256:fedcba9876543210\"
        - name: proxy
          image: nginx:1.27
",
        );
        let kust_text = "\
apiVersion: kustomize.config.k8s.io/v1beta1
kind: Kustomization
resources:
  - deployment.yaml
images:
  - name: 
";
        host.add_file("kustomize-reg/kustomization.yaml", kust_text);

        let mut plugin = KubernetesPlugin::default();
        plugin
            .activate(&host.context("dengliu.kubernetes"))
            .unwrap();

        let doc = Document {
            path: "/project/kustomize-reg/kustomization.yaml".into(),
            language: "yaml".into(),
            version: 1,
            text: kust_text.into(),
        };
        let labels: Vec<String> = plugin
            .completion(&doc, Position::new(5, 10), None)
            .into_iter()
            .map(|i| i.label)
            .collect();
        assert!(
            labels.contains(&"localhost:5000/team/app".to_owned()),
            "expected localhost:5000/team/app, got {labels:?}"
        );
        assert!(
            !labels.contains(&"localhost".to_owned()),
            "must not truncate at registry port colon: {labels:?}"
        );
        assert!(labels.contains(&"registry.corp.internal:8443/ns/worker".to_owned()));
        assert!(labels.contains(&"[::1]:5000/org/svc".to_owned()));
        assert!(labels.contains(&"nginx".to_owned()));

        // Typing past the port colon (`name: localhost:5000/`) still matches and replaces the full image name
        let typed_doc = Document {
            text: "\
apiVersion: kustomize.config.k8s.io/v1beta1
kind: Kustomization
resources:
  - deployment.yaml
images:
  - name: localhost:5000/
"
            .into(),
            ..doc
        };
        let port_typed = plugin.completion(&typed_doc, Position::new(5, 25), None);
        assert_eq!(port_typed.len(), 1);
        assert_eq!(port_typed[0].label, "localhost:5000/team/app");
        assert_eq!(
            port_typed[0].replace,
            Some(Range::new(Position::new(5, 10), Position::new(5, 25)))
        );
    }

    #[test]
    fn helm_range_header_variables_stay_in_scope_through_else_and_restore_at_end() {
        let host = MockHost::install();
        host.add_file(
            "range-else-chart/Chart.yaml",
            "apiVersion: v2\nname: range-else-chart\nversion: 0.1.0\n",
        );
        host.add_file(
            "range-else-chart/values.yaml",
            "outer: {name: OUTER, outer_only: true}\nitems: []\n",
        );

        // 1. Shadowed `$cfg` in `range ... else ... end` (exact Go template variable scope rule)
        let shadowed_tpl = "\
{{- $cfg := .Values.outer -}}
{{- range $cfg := .Values.items -}}
loop: {{ $cfg }}
{{- else -}}
empty: {{ $cfg }}
{{- end -}}
after: {{ $cfg }}
";
        host.add_file("range-else-chart/templates/shadowed.yaml", shadowed_tpl);

        // 2. Unshadowed `$idx, $entry` + branch-local `$loop_only` in `range ... else ... end`
        let unshadowed_tpl = "\
{{- range $idx, $entry := .Values.items -}}
{{- $loop_only := .Values.outer -}}
loop: {{ $idx }}={{ $entry }}
{{- else -}}
empty: {{ $idx }}={{ $entry }}
{{- end -}}
after: done
";
        host.add_file("range-else-chart/templates/unshadowed.yaml", unshadowed_tpl);

        let mut plugin = KubernetesPlugin::default();
        plugin
            .activate(&host.context("dengliu.kubernetes"))
            .unwrap();

        let shadowed_doc = Document {
            path: "/project/range-else-chart/templates/shadowed.yaml".into(),
            language: "helm-template".into(),
            version: 1,
            text: shadowed_tpl.into(),
        };

        // Loop body `$cfg` (line 2, col 11) -> range declaration (line 1)
        let loop_defs: Vec<u32> = plugin
            .definition(&shadowed_doc, Position::new(2, 11))
            .into_iter()
            .filter(|d| d.path == shadowed_doc.path)
            .map(|d| d.range.start.line)
            .collect();
        assert_eq!(loop_defs, vec![1]);

        // `else` branch `$cfg` (line 4, col 12) -> range declaration (line 1), NOT outer line 0!
        let else_defs: Vec<u32> = plugin
            .definition(&shadowed_doc, Position::new(4, 12))
            .into_iter()
            .filter(|d| d.path == shadowed_doc.path)
            .map(|d| d.range.start.line)
            .collect();
        assert_eq!(else_defs, vec![1]);

        // Post-`end` `$cfg` (line 6, col 12) -> outer declaration (line 0)
        let after_defs: Vec<u32> = plugin
            .definition(&shadowed_doc, Position::new(6, 12))
            .into_iter()
            .filter(|d| d.path == shadowed_doc.path)
            .map(|d| d.range.start.line)
            .collect();
        assert_eq!(after_defs, vec![0]);

        // In `else`, `$cfg` holds the zero-iteration empty value, so `$cfg.` does NOT leak `outer_only`
        let else_field_doc = Document {
            text: "\
{{- $cfg := .Values.outer -}}
{{- range $cfg := .Values.items -}}
loop: {{ $cfg }}
{{- else -}}
empty: {{ $cfg.
{{- end -}}
"
            .into(),
            ..shadowed_doc.clone()
        };
        let else_field_items = plugin.completion(&else_field_doc, Position::new(4, 15), Some("."));
        assert!(
            else_field_items.is_empty(),
            "zero-iteration range $cfg in else must not leak outer fields, got {else_field_items:?}"
        );

        // After `end`, `$cfg.` restores outer `.Values.outer` (including inline flow mapping keys!)
        let after_field_doc = Document {
            text: "\
{{- $cfg := .Values.outer -}}
{{- range $cfg := .Values.items -}}
loop: {{ $cfg }}
{{- else -}}
empty: {{ $cfg }}
{{- end -}}
after: {{ $cfg.
"
            .into(),
            ..shadowed_doc
        };
        let after_field_labels: Vec<String> = plugin
            .completion(&after_field_doc, Position::new(6, 15), Some("."))
            .into_iter()
            .map(|i| i.label)
            .collect();
        assert!(
            after_field_labels.contains(&"name".to_owned())
                && after_field_labels.contains(&"outer_only".to_owned()),
            "expected outer flow-map keys after end, got {after_field_labels:?}"
        );

        // Unshadowed `$idx` and `$entry` in `else`: definition jumps to line 0, completion offers both
        // while omitting branch-local `$loop_only`!
        let unshadowed_doc = Document {
            path: "/project/range-else-chart/templates/unshadowed.yaml".into(),
            language: "helm-template".into(),
            version: 1,
            text: unshadowed_tpl.into(),
        };
        let else_idx_defs: Vec<u32> = plugin
            .definition(&unshadowed_doc, Position::new(4, 12))
            .into_iter()
            .filter(|d| d.path == unshadowed_doc.path)
            .map(|d| d.range.start.line)
            .collect();
        assert_eq!(else_idx_defs, vec![0]);

        let else_entry_defs: Vec<u32> = plugin
            .definition(&unshadowed_doc, Position::new(4, 24))
            .into_iter()
            .filter(|d| d.path == unshadowed_doc.path)
            .map(|d| d.range.start.line)
            .collect();
        assert_eq!(else_entry_defs, vec![0]);

        let else_vars_doc = Document {
            text: "\
{{- range $idx, $entry := .Values.items -}}
{{- $loop_only := .Values.outer -}}
loop: {{ $idx }}={{ $entry }}
{{- else -}}
empty: {{ $
{{- end -}}
"
            .into(),
            ..unshadowed_doc.clone()
        };
        let else_var_labels: Vec<String> = plugin
            .completion(&else_vars_doc, Position::new(4, 11), Some("$"))
            .into_iter()
            .map(|i| i.label)
            .collect();
        assert!(
            else_var_labels.contains(&"$idx".to_owned())
                && else_var_labels.contains(&"$entry".to_owned()),
            "expected range header vars in else completion, got {else_var_labels:?}"
        );
        assert!(
            !else_var_labels.contains(&"$loop_only".to_owned()),
            "loop-body var must not leak into else: {else_var_labels:?}"
        );

        // After `end`, neither `$idx` nor `$entry` remains in scope
        let post_end_vars_doc = Document {
            text: "\
{{- range $idx, $entry := .Values.items -}}
loop: {{ $entry }}
{{- else -}}
empty: {{ $entry }}
{{- end -}}
after: {{ $
"
            .into(),
            ..unshadowed_doc
        };
        let post_end_labels: Vec<String> = plugin
            .completion(&post_end_vars_doc, Position::new(5, 11), Some("$"))
            .into_iter()
            .map(|i| i.label)
            .collect();
        assert!(
            !post_end_labels.contains(&"$idx".to_owned())
                && !post_end_labels.contains(&"$entry".to_owned()),
            "range header vars leaked past end: {post_end_labels:?}"
        );
    }

    #[test]
    fn helm_flow_mapping_values_with_trailing_comments_complete_and_navigate() {
        let host = MockHost::install();
        host.add_file(
            "flow-chart/Chart.yaml",
            "apiVersion: v2\nname: flow-chart\nversion: 0.1.0\n",
        );
        let values_text = "\
outer: {name: OUTER, nested: {key: VALUE}} # documented settings
meta: {label: \"app #1\", env: 'it''s #prod'} # quoted hash inside flow map
rawExpr: \"{literal: text}\" # quoted scalar, not a flow map
";
        host.add_file("flow-chart/values.yaml", values_text);
        let tpl_text = "val: {{ .Values.outer.nested.key }}\n";
        host.add_file("flow-chart/templates/configmap.yaml", tpl_text);

        let mut plugin = KubernetesPlugin::default();
        plugin
            .activate(&host.context("dengliu.kubernetes"))
            .unwrap();

        let tpl_doc = Document {
            path: "/project/flow-chart/templates/configmap.yaml".into(),
            language: "helm-template".into(),
            version: 1,
            text: tpl_text.into(),
        };

        // 1. Go to Definition on `key` in `{{ .Values.outer.nested.key }}` (line 0, col 30)
        //    jumps to `key` (line 0, cols 30..33) inside `outer: {name: OUTER, nested: {key: VALUE}} # documented settings`
        let key_defs = plugin.definition(&tpl_doc, Position::new(0, 30));
        assert_eq!(
            key_defs.len(),
            1,
            "expected target in values.yaml, got {key_defs:?}"
        );
        assert!(key_defs[0].path.ends_with("flow-chart/values.yaml"));
        assert_eq!(
            key_defs[0].range,
            Range::new(Position::new(0, 30), Position::new(0, 33))
        );

        // 2. Completion after `.Values.outer.` offers `name` and `nested` despite trailing `# documented settings`
        let outer_comp_doc = Document {
            text: "val: {{ .Values.outer.\n".into(),
            ..tpl_doc.clone()
        };
        let mut outer_labels: Vec<String> = plugin
            .completion(&outer_comp_doc, Position::new(0, 22), Some("."))
            .into_iter()
            .map(|i| i.label)
            .collect();
        outer_labels.sort();
        assert_eq!(outer_labels, vec!["name".to_owned(), "nested".to_owned()]);

        // 3. Completion after `.Values.outer.nested.` offers `key`
        let nested_comp_doc = Document {
            text: "val: {{ .Values.outer.nested.\n".into(),
            ..tpl_doc.clone()
        };
        let nested_labels: Vec<String> = plugin
            .completion(&nested_comp_doc, Position::new(0, 29), Some("."))
            .into_iter()
            .map(|i| i.label)
            .collect();
        assert_eq!(nested_labels, vec!["key".to_owned()]);

        // 4. Quoted `#` inside flow values (`meta: {label: "app #1", env: 'it''s #prod'} # ...`) preserves all keys,
        //    while quoted string scalar `rawExpr: "{literal: text}"` is NOT treated as a flow map.
        let meta_comp_doc = Document {
            text: "val: {{ .Values.meta.\n".into(),
            ..tpl_doc.clone()
        };
        let mut meta_labels: Vec<String> = plugin
            .completion(&meta_comp_doc, Position::new(0, 21), Some("."))
            .into_iter()
            .map(|i| i.label)
            .collect();
        meta_labels.sort();
        assert_eq!(meta_labels, vec!["env".to_owned(), "label".to_owned()]);

        let raw_comp_doc = Document {
            text: "val: {{ .Values.rawExpr.\n".into(),
            ..tpl_doc.clone()
        };
        assert!(
            plugin
                .completion(&raw_comp_doc, Position::new(0, 24), Some("."))
                .is_empty(),
            "quoted scalar starting with '{{' must not be parsed as a YAML flow mapping"
        );

        // 5. Restore template in index and verify reverse navigation from `key` (col 31) in `values.yaml`
        let _ = plugin.line_markers(&tpl_doc);
        let values_doc = Document {
            path: "/project/flow-chart/values.yaml".into(),
            language: "yaml".into(),
            version: 1,
            text: values_text.into(),
        };
        let rev_defs = plugin.definition(&values_doc, Position::new(0, 31));
        assert!(
            rev_defs
                .iter()
                .any(|d| d.path == tpl_doc.path && d.range.start.line == 0),
            "expected reverse link from nested flow key to template, got {rev_defs:?}"
        );
    }
}
