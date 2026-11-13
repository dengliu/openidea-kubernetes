import { describe, expect, it } from "vitest";
import {
  applyWatchDeltaToSnapshot,
  classifyIncomingNamespaceSnapshot,
  computeNamespaceScopeSignature,
  computePollDelayMs,
  computeVirtualWindow,
  formatCompactAge,
  formatGroundTruthSearchSummary,
  formatNamespaceButtonLabel,
  formatPaginationStatus,
  isPluginWindowActive,
  isResourceInNamespaceScope,
  matchesEventSearch,
  matchesResourceSearch,
  NS_OPTION_ROW_HEIGHT,
  NS_VIEWPORT_HEIGHT,
  pruneSnapshotToNamespaceScope,
  resolveSelectedNamespaces,
  shouldRunClusterGroundTruthSearch,
  shouldTriggerAutoLoadMore,
  SOFT_AUTO_LOAD_CAP,
  toggleNamespaceInSelection,
  VIRTUAL_ROW_HEIGHT,
} from "./App";
import { bytesToUnits, cpuUnitsToNumber, formatKubeMemory, toYamlLines, unitsToBytes } from "./ResourceDetails";

describe("ResourceDetails helpers", () => {
  it("computes virtualized table window with top and bottom spacers on large row sets", () => {
    expect(computeVirtualWindow(40, 0, 600)).toEqual({
      start: 0,
      end: 40,
      topPadPx: 0,
      bottomPadPx: 0,
    });
    const win = computeVirtualWindow(10000, VIRTUAL_ROW_HEIGHT * 500, 740, VIRTUAL_ROW_HEIGHT, 20);
    expect(win.start).toBe(480);
    expect(win.end).toBe(540);
    expect(win.topPadPx).toBe(480 * VIRTUAL_ROW_HEIGHT);
    expect(win.bottomPadPx).toBe((10000 - 540) * VIRTUAL_ROW_HEIGHT);

    // Virtualizes 1,007 namespaces in the Namespace Picker dropdown without truncating tail items
    const nsTailWin = computeVirtualWindow(
      1007,
      1007 * NS_OPTION_ROW_HEIGHT - NS_VIEWPORT_HEIGHT,
      NS_VIEWPORT_HEIGHT,
      NS_OPTION_ROW_HEIGHT,
      10,
      50,
    );
    expect(nsTailWin.end).toBe(1007);
    expect(nsTailWin.end - nsTailWin.start).toBeLessThanOrEqual(40);
    expect(nsTailWin.bottomPadPx).toBe(0);
  });

  it("formats server-side pagination ranges and totals across replace and append modes", () => {
    const p1 = formatPaginationStatus(
      {
        category: "Pod",
        limit: 100,
        continue_token: "tok-2",
        remaining_count: 52710,
        loaded_count: 100,
        page_index: 1,
        mode: "replace",
      },
      100,
      100,
      false,
    );
    expect(p1.subheaderText).toBe("1–100 of 52,810 items");
    expect(p1.totalPages).toBe(529);
    expect(p1.hasMore).toBe(true);

    const p3Filtered = formatPaginationStatus(
      {
        category: "Pod",
        limit: 100,
        continue_token: "tok-4",
        remaining_count: 52510,
        loaded_count: 100,
        page_index: 3,
        mode: "replace",
      },
      7,
      100,
      true,
    );
    expect(p3Filtered.subheaderText).toBe("7 matching (201–300 of 52,810)");

    const appended = formatPaginationStatus(
      {
        category: "Event",
        limit: 100,
        continue_token: "ev-tok",
        remaining_count: 133648,
        loaded_count: 300,
        page_index: 3,
        mode: "append",
      },
      300,
      300,
      false,
    );
    expect(appended.subheaderText).toBe("300 of 133,948 loaded");
    expect(appended.isAppend).toBe(true);
  });

  it("applies watch-delta upserts and deletions in place while preserving cached CPU/Memory metrics", () => {
    const base = {
      connected: true,
      current_context: "falcon-phx-ca",
      contexts: ["falcon-phx-ca"],
      cluster_catalog: [],
      current_namespace: "production",
      all_namespaces: false,
      namespaces: ["default", "production"],
      resources: [
        {
          kind: "Pod",
          name: "api-0",
          namespace: "production",
          status: "Running",
          ready: "1/1",
          age: "10m",
          detail: "",
          containers: ["web"],
          ports: [],
          data_keys: [],
          cpu: "0.250",
          memory: "512.0Mi",
          restarts: 0,
        },
        {
          kind: "Pod",
          name: "old-job-0",
          namespace: "production",
          status: "Succeeded",
          ready: "0/1",
          age: "20m",
          detail: "",
          containers: ["job"],
          ports: [],
          data_keys: [],
        },
      ],
      events: [],
      helm_releases: [],
      page_info: {
        Pod: {
          category: "Pod",
          limit: 100,
          continue_token: "",
          loaded_count: 2,
          page_index: 1,
          mode: "replace",
          resource_version: "1001",
          watch_supported: true,
        },
      },
      overview_counts: { Pod: 2 },
    };

    const next = applyWatchDeltaToSnapshot(base, {
      category: "Pod",
      upserts: [
        {
          kind: "Pod",
          name: "api-0",
          namespace: "production",
          status: "CrashLoopBackOff",
          ready: "0/1",
          age: "11m",
          detail: "",
          containers: ["web"],
          ports: [],
          data_keys: [],
          cpu: "",
          memory: "",
          restarts: 3,
        },
        {
          kind: "Pod",
          name: "api-1",
          namespace: "production",
          status: "Running",
          ready: "1/1",
          age: "2s",
          detail: "",
          containers: ["web"],
          ports: [],
          data_keys: [],
          restarts: 0,
        },
      ],
      deleted: [{ kind: "Pod", namespace: "production", name: "old-job-0" }],
    });

    expect(next.resources.map((r) => r.name)).toEqual(["api-0", "api-1"]);
    expect(next.resources[0].status).toBe("CrashLoopBackOff");
    expect(next.resources[0].restarts).toBe(3);
    expect(next.resources[0].cpu).toBe("0.250");
    expect(next.resources[0].memory).toBe("512.0Mi");
    expect(next.overview_counts?.Pod).toBe(2);
    expect(next.page_info?.Pod.loaded_count).toBe(2);
  });

  it("preserves server-side table multi-unit duration strings in formatCompactAge", () => {
    expect(formatCompactAge("6d22h")).toBe("6d22h");
    expect(formatCompactAge("3d5h10m")).toBe("3d5h10m");
    expect(formatCompactAge("45s")).toBe("45s");
  });

  it("toggles multiple namespaces cleanly and formats button labels", () => {
    expect(
      resolveSelectedNamespaces({
        all_namespaces: false,
        selected_namespaces: ["dayan", "dengliu"],
        current_namespace: "dayan",
      }),
    ).toEqual(["dayan", "dengliu"]);

    // Starting from All Namespaces, clicking `dayan` selects `["dayan"]`
    const s1 = toggleNamespaceInSelection([], true, "dayan");
    expect(s1).toEqual({ allNamespaces: false, selectedNamespaces: ["dayan"] });

    // Clicking `dengliu` and `default` builds a sorted multi-namespace selection
    const s2 = toggleNamespaceInSelection(s1.selectedNamespaces, s1.allNamespaces, "dengliu");
    expect(s2).toEqual({ allNamespaces: false, selectedNamespaces: ["dayan", "dengliu"] });
    expect(formatNamespaceButtonLabel(false, s2.selectedNamespaces).label).toBe("Namespaces: dayan, dengliu");

    const s3 = toggleNamespaceInSelection(s2.selectedNamespaces, s2.allNamespaces, "default");
    expect(s3).toEqual({ allNamespaces: false, selectedNamespaces: ["dayan", "default", "dengliu"] });
    expect(formatNamespaceButtonLabel(false, s3.selectedNamespaces)).toEqual({
      label: "Namespaces: dayan, default +1",
      tooltip: "Namespaces (3): dayan, default, dengliu",
    });

    // Unchecking all remaining namespaces smoothly falls back to All Namespaces
    const s4 = toggleNamespaceInSelection(["dayan"], false, "dayan");
    expect(s4).toEqual({ allNamespaces: true, selectedNamespaces: [] });
    expect(formatNamespaceButtonLabel(true, []).label).toBe("Namespace: All Namespaces");
  });

  it("triggers predictive auto-load near scroll bottom while guarding in-flight, soft cap, and short search viewports", () => {
    // Far from bottom -> false
    expect(
      shouldTriggerAutoLoadMore({
        hasMore: true,
        isLoading: false,
        hasSearch: false,
        rawLoadedCount: 200,
        scrollTop: 800,
        viewportHeight: 700,
        scrollHeight: 7400,
      }),
    ).toBe(false);

    // Within predictive prefetch zone (~15 rows from bottom) -> true
    expect(
      shouldTriggerAutoLoadMore({
        hasMore: true,
        isLoading: false,
        hasSearch: false,
        rawLoadedCount: 200,
        scrollTop: 6300,
        viewportHeight: 700,
        scrollHeight: 7400,
      }),
    ).toBe(true);

    // Already loading or hit soft memory cap (3,000 rows) -> false
    expect(
      shouldTriggerAutoLoadMore({
        hasMore: true,
        isLoading: true,
        hasSearch: false,
        rawLoadedCount: 200,
        scrollTop: 6300,
        viewportHeight: 700,
        scrollHeight: 7400,
      }),
    ).toBe(false);
    expect(
      shouldTriggerAutoLoadMore({
        hasMore: true,
        isLoading: false,
        hasSearch: false,
        rawLoadedCount: SOFT_AUTO_LOAD_CAP,
        scrollTop: 99000,
        viewportHeight: 700,
        scrollHeight: 100000,
      }),
    ).toBe(false);

    // Active search query that shrank filtered rows below viewport height -> false (prevents runaway auto-scan loop)
    expect(
      shouldTriggerAutoLoadMore({
        hasMore: true,
        isLoading: false,
        hasSearch: true,
        rawLoadedCount: 200,
        scrollTop: 0,
        viewportHeight: 700,
        scrollHeight: 120,
      }),
    ).toBe(false);
  });

  it("adapts background poll interval to cluster latency and resource count", () => {
    expect(computePollDelayMs(150, 40)).toBe(5000);
    expect(computePollDelayMs(950, 40)).toBe(10000);
    expect(computePollDelayMs(120, 600)).toBe(10000);
    expect(computePollDelayMs(2400, 100)).toBe(15000);
    expect(computePollDelayMs(200, 2000)).toBe(15000);
  });
  it("converts Kubernetes binary and decimal memory units to bytes and GiB/MiB", () => {
    expect(unitsToBytes("1024Ki")).toBe(1024 * 1024);
    expect(unitsToBytes("2Gi")).toBe(2 * 1024 ** 3);
    expect(bytesToUnits(unitsToBytes("65855956Ki"))).toBe("62.8GiB");
    expect(formatKubeMemory("512Mi").display).toBe("512.0MiB");
  });

  it("parses CPU cores from millicores, nanocores, and whole cores", () => {
    expect(cpuUnitsToNumber("250m")).toBeCloseTo(0.25);
    expect(cpuUnitsToNumber("500000000n")).toBeCloseTo(0.5);
    expect(cpuUnitsToNumber("4")).toBe(4);
  });

  it("formats nested Kubernetes objects into 2-space indented YAML lines", () => {
    const affinity = {
      nodeAffinity: {
        requiredDuringSchedulingIgnoredDuringExecution: {
          nodeSelectorTerms: [
            {
              matchExpressions: [
                {
                  key: "kubernetes.io/os",
                  operator: "In",
                  values: ["linux"],
                },
              ],
            },
          ],
        },
      },
    };
    const lines = toYamlLines(affinity);
    expect(lines).toEqual([
      "nodeAffinity:",
      "  requiredDuringSchedulingIgnoredDuringExecution:",
      "    nodeSelectorTerms:",
      "    - matchExpressions:",
      "      - key: kubernetes.io/os",
      "        operator: In",
      "        values:",
      "        - linux",
    ]);
  });

  it("triggers cluster ground-truth search only when a paged category is partially loaded", () => {
    const partialPods = {
      category: "Pod",
      limit: 200,
      continue_token: "p2-tok",
      remaining_count: 45958,
      loaded_count: 200,
      page_index: 1,
      mode: "replace" as const,
    };
    const fullyLoadedPods = {
      ...partialPods,
      continue_token: "",
      remaining_count: 0,
      loaded_count: 320,
    };

    expect(shouldRunClusterGroundTruthSearch("Pod", "brix", partialPods)).toBe(true);
    expect(shouldRunClusterGroundTruthSearch("Event", "BackOff", { ...partialPods, category: "Event" })).toBe(true);
    expect(shouldRunClusterGroundTruthSearch("CR:workloads.kueue.x-k8s.io", "gpu", partialPods)).toBe(true);
    expect(shouldRunClusterGroundTruthSearch("Pod", "   ", partialPods)).toBe(false);
    expect(shouldRunClusterGroundTruthSearch("Pod", "brix", fullyLoadedPods)).toBe(false);
    expect(shouldRunClusterGroundTruthSearch("Workspace", "brix", partialPods)).toBe(false);
    expect(shouldRunClusterGroundTruthSearch("HelmRelease", "brix", partialPods)).toBe(false);
  });

  it("matches multi-token, namespace/name, and structured queries consistently with backend search", () => {
    const pod = {
      kind: "Pod",
      name: "brix-runner-7f8b9",
      namespace: "dayan",
      status: "CrashLoopBackOff",
      ready: "0/1",
      age: "12m",
      detail: "restarts: 4 • 10.244.99.42 on aks-gpu-77",
      containers: ["brix"],
      ports: [],
      data_keys: [],
      pod_ip: "10.244.99.42",
      node_name: "aks-gpu-77",
    };
    const knownNs = new Set(["dayan", "adx-mon", "kube-system"]);
    expect(matchesResourceSearch(pod, "dayan/brix", knownNs)).toBe(true);
    expect(matchesResourceSearch(pod, "adx-mon/brix", knownNs)).toBe(false);
    expect(matchesResourceSearch(pod, "dayan crashloop", knownNs)).toBe(true);
    expect(matchesResourceSearch(pod, "node:gpu-77 status:crashloop ip:10.244.99", knownNs)).toBe(true);
    expect(matchesResourceSearch(pod, "node:cpu-01", knownNs)).toBe(false);

    const ev = {
      kind: "Pod",
      object: "brix-runner-7f8b9",
      namespace: "dayan",
      reason: "BackOff",
      message: "Back-off restarting failed container brix",
      event_type: "Warning",
      count: 3,
      last_seen: "12s",
    };
    expect(matchesEventSearch(ev, "ns:dayan restarting failed")).toBe(true);
    expect(matchesEventSearch(ev, "status:warning brix")).toBe(true);
    expect(matchesEventSearch(ev, "ns:kube-system brix")).toBe(false);
  });

  it("formats ground-truth search subheader summaries for loading, exact, and truncated results", () => {
    expect(
      formatGroundTruthSearchSummary(
        { loading: true, totalMatches: 0, scannedTotal: 0, truncated: false, error: null },
        0,
        46158,
      ),
    ).toBe("Searching 46,158 items…");
    expect(
      formatGroundTruthSearchSummary(
        { loading: false, totalMatches: 14, scannedTotal: 46158, truncated: false, error: null },
        14,
        46158,
      ),
    ).toBe("14 matches across 46,158 items");
    expect(
      formatGroundTruthSearchSummary(
        { loading: false, totalMatches: 1, scannedTotal: 46158, truncated: false, error: null },
        1,
        46158,
      ),
    ).toBe("1 match across 46,158 items");
    expect(
      formatGroundTruthSearchSummary(
        { loading: false, totalMatches: 1840, scannedTotal: 46158, truncated: true, error: null },
        500,
        46158,
      ),
    ).toBe("First 500 of 1,840 matches across 46,158 items");
  });

  it("prunes out-of-scope namespaces immediately and rejects stale in-flight poll snapshots", () => {
    const baseSnap = {
      connected: true,
      current_context: "falcon-phx-ca",
      contexts: ["falcon-phx-ca"],
      cluster_catalog: [],
      current_namespace: "default",
      all_namespaces: true,
      selected_namespaces: ["default"],
      namespaces: ["default", "kube-system"],
      resources: [
        {
          kind: "Pod",
          name: "abhaldar-fc-sydney",
          namespace: "abhaldar",
          status: "Failed",
          ready: "0/4",
          age: "3d",
          detail: "",
          containers: [],
          ports: [],
          data_keys: [],
        },
        {
          kind: "Pod",
          name: "brix-git",
          namespace: "dengliu",
          status: "Running",
          ready: "1/1",
          age: "5d",
          detail: "",
          containers: [],
          ports: [],
          data_keys: [],
        },
        {
          kind: "CustomResourceDefinition",
          name: "pools.falcon.io",
          namespace: "",
          status: "Namespaced",
          ready: "falcon.io",
          age: "90d",
          detail: "",
          containers: [],
          ports: [],
          data_keys: [],
        },
      ],
      events: [
        {
          kind: "Pod",
          object: "abhaldar-fc-sydney",
          namespace: "abhaldar",
          reason: "BackOff",
          message: "failed",
          event_type: "Warning",
          count: 1,
          last_seen: "1m",
        },
        {
          kind: "Pod",
          object: "brix-git",
          namespace: "dengliu",
          reason: "Started",
          message: "ok",
          event_type: "Normal",
          count: 1,
          last_seen: "2m",
        },
      ],
      helm_releases: [
        {
          name: "other-rel",
          namespace: "abhaldar",
          revision: "1",
          status: "deployed",
          chart: "app-1.0",
          app_version: "1.0",
          updated: "1d",
        },
      ],
      overview_counts: { Namespace: 952, Pod: 50000 },
    };

    // 1. Optimistic switch to `dengliu` removes `abhaldar` rows immediately while preserving CRDs and discovered namespace names
    const pruned = pruneSnapshotToNamespaceScope(baseSnap, {
      allNamespaces: false,
      selectedNamespaces: ["dengliu"],
    });
    expect(pruned.all_namespaces).toBe(false);
    expect(pruned.selected_namespaces).toEqual(["dengliu"]);
    expect(pruned.current_namespace).toEqual("dengliu");
    expect(pruned.resources.map((r) => `${r.kind}:${r.namespace}/${r.name}`)).toEqual([
      "Pod:dengliu/brix-git",
      "CustomResourceDefinition:/pools.falcon.io",
    ]);
    expect(pruned.events.map((e) => e.namespace)).toEqual(["dengliu"]);
    expect(pruned.helm_releases).toEqual([]);
    expect(pruned.namespaces).toContain("abhaldar");
    expect(pruned.namespaces).toContain("dengliu");
    expect(pruned.overview_counts).toEqual({ Namespace: 952 });

    // 2. Scope check helper filters namespaced kinds strictly while keeping cluster-scoped kinds
    const dengSet = new Set(["dengliu"]);
    expect(isResourceInNamespaceScope({ kind: "Pod", namespace: "abhaldar" }, false, dengSet)).toBe(false);
    expect(isResourceInNamespaceScope({ kind: "Pod", namespace: "dengliu" }, false, dengSet)).toBe(true);
    expect(isResourceInNamespaceScope({ kind: "Node", namespace: "" }, false, dengSet)).toBe(true);
    expect(isResourceInNamespaceScope({ kind: "CR:clusterpolicies.kyverno.io", namespace: "" }, false, dengSet)).toBe(
      true,
    );
    expect(isResourceInNamespaceScope({ kind: "CR:pools.falcon.io", namespace: "abhaldar" }, false, dengSet)).toBe(
      false,
    );

    // 3. Stale `All Namespaces` background poll arriving while `dengliu` switch is in flight is classified as `"stale"`
    const guard = {
      context: "falcon-phx-ca",
      signature: computeNamespaceScopeSignature(false, ["dengliu"]),
      sentAt: 1000,
    };
    expect(classifyIncomingNamespaceSnapshot(baseSnap, true, guard, false, 1500)).toBe("stale");
    expect(classifyIncomingNamespaceSnapshot(pruned, false, guard, false, 1800)).toBe("accept_and_clear_guard");

    // 4. Watch deltas from another namespace are ignored when scoped to `dengliu`
    const afterStaleWatch = applyWatchDeltaToSnapshot(pruned, {
      category: "Pod",
      upserts: [
        {
          kind: "Pod",
          name: "intruder-pod",
          namespace: "abhaldar",
          status: "Running",
          ready: "1/1",
          age: "1s",
          detail: "",
          containers: [],
          ports: [],
          data_keys: [],
        },
      ],
    });
    expect(afterStaleWatch.resources.some((r) => r.namespace === "abhaldar")).toBe(false);
  });

  it("pauses background polling when the desktop window is unfocused or hidden", () => {
    expect(isPluginWindowActive({ visibilityState: "visible", hasFocus: () => true })).toBe(true);
    expect(isPluginWindowActive({ visibilityState: "visible", hasFocus: () => false })).toBe(false);
    expect(isPluginWindowActive({ visibilityState: "hidden", hasFocus: () => true })).toBe(false);
    expect(isPluginWindowActive(undefined)).toBe(true);
  });
});
