import { describe, expect, it } from "vitest";
import { computePollDelayMs, computeVirtualWindow, formatCompactAge, VIRTUAL_ROW_HEIGHT } from "./App";
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
