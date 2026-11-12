import { describe, expect, it } from "vitest";
import {
  applyWatchDeltaToSnapshot,
  computePollDelayMs,
  computeVirtualWindow,
  formatCompactAge,
  formatNamespaceButtonLabel,
  formatPaginationStatus,
  NS_OPTION_ROW_HEIGHT,
  NS_VIEWPORT_HEIGHT,
  resolveSelectedNamespaces,
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
});
