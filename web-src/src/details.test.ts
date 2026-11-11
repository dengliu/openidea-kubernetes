import { describe, expect, it } from "vitest";
import {
  computePollDelayMs,
  computeVirtualWindow,
  formatCompactAge,
  formatPaginationStatus,
  NS_OPTION_ROW_HEIGHT,
  NS_VIEWPORT_HEIGHT,
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

  it("preserves server-side table multi-unit duration strings in formatCompactAge", () => {
    expect(formatCompactAge("6d22h")).toBe("6d22h");
    expect(formatCompactAge("3d5h10m")).toBe("3d5h10m");
    expect(formatCompactAge("45s")).toBe("45s");
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
