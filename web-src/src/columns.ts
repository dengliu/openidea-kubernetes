import type { ColumnDef } from "./types";

export interface ColumnLayout {
  order?: string[];
  visible?: Record<string, boolean>;
  widths?: Record<string, number>;
}

export type TextMeasurer = (text: string, medium?: boolean) => number;

const CELL_H_PAD = 14;
const HEADER_SORT_EXTRA = 12;
const AUTO_FIT_SAMPLE_LIMIT = 100;

let cachedMeasureCanvas: CanvasRenderingContext2D | null | undefined;

export function createDomTextMeasurer(): TextMeasurer {
  return (text: string, medium = false) => {
    if (!text) return 0;
    if (cachedMeasureCanvas === undefined) {
      if (typeof document !== "undefined") {
        try {
          cachedMeasureCanvas = document.createElement("canvas").getContext("2d");
        } catch {
          cachedMeasureCanvas = null;
        }
      } else {
        cachedMeasureCanvas = null;
      }
    }
    const ctx = cachedMeasureCanvas;
    if (ctx) {
      const style = typeof getComputedStyle === "function" && document.body ? getComputedStyle(document.body) : null;
      const fontSize = style?.fontSize || "12.5px";
      const fontFamily = style?.fontFamily || '-apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif';
      ctx.font = `${medium ? 500 : 400} ${fontSize} ${fontFamily}`;
      return ctx.measureText(text).width;
    }
    return text.length * (medium ? 7.4 : 7.0);
  };
}

function maxAutoWidthFor(col: ColumnDef): number {
  switch (col.id) {
    case "warning":
      return 28;
    case "message":
      return 340;
    case "name":
    case "crd_resource":
    case "object":
      return Math.max(col.defaultWidth, 275);
    case "cm_keys":
    case "ns_labels":
    case "labels":
    case "detail":
    case "path":
      return Math.max(col.defaultWidth, 240);
    case "node":
    case "node_kernel":
    case "svc_type":
      return Math.max(col.defaultWidth, 200);
    case "wl_conditions":
      return Math.max(col.defaultWidth, 165);
    default:
      return Math.max(col.defaultWidth, 150);
  }
}

export function autoFitColumnWidths(
  defs: ColumnDef[],
  rowCount: number,
  measureCell: (colId: string, rowIdx: number, measure: TextMeasurer) => number,
  measure: TextMeasurer = createDomTextMeasurer(),
  sortCol?: string,
): Record<string, number> {
  if (rowCount <= 0) return {};
  const sampleCount = Math.min(rowCount, AUTO_FIT_SAMPLE_LIMIT);
  const result: Record<string, number> = {};
  for (const col of defs) {
    if (col.id === "warning") {
      result[col.id] = 28;
      continue;
    }
    const sortExtra = col.id === sortCol ? HEADER_SORT_EXTRA : 2;
    const headerW = Math.ceil(measure(col.label, true)) + CELL_H_PAD + sortExtra;
    let contentW = 0;
    for (let i = 0; i < sampleCount; i++) {
      const w = measureCell(col.id, i, measure);
      if (w > contentW) contentW = w;
    }
    const maxW = maxAutoWidthFor(col);
    const minW = Math.max(38, Math.min(headerW, maxW));
    result[col.id] = Math.min(maxW, Math.max(minW, Math.ceil(contentW)));
  }
  return result;
}

export function tableColumns(
  defs: ColumnDef[],
  layout: ColumnLayout,
  viewportWidth: number,
  autoWidths?: Record<string, number>,
) {
  const pinned = defs.find((c) => c.id === "name" || c.id === "crd_resource") ?? defs[0];
  const remaining = new Map(defs.map((c) => [c.id, c]));
  const ordered: ColumnDef[] = [];
  for (const id of [pinned.id, ...(layout.order ?? [])]) {
    const col = remaining.get(id);
    if (col) {
      ordered.push(col);
      remaining.delete(id);
    }
  }
  ordered.push(...remaining.values());
  const visible = ordered.filter((col) => col === pinned || (layout.visible?.[col.id] ?? col.defaultVisible));
  const preferredOf = (col: ColumnDef) => {
    const minCol = col.id === "warning" ? 28 : 36;
    const raw = layout.widths?.[col.id] ?? autoWidths?.[col.id] ?? col.defaultWidth;
    return Math.max(minCol, raw);
  };
  const uncappedTotal = visible.reduce((sum, col) => sum + preferredOf(col), 0);
  const capPinned = viewportWidth > 0 && uncappedTotal > viewportWidth;
  const width = (col: ColumnDef) => {
    const preferred = preferredOf(col);
    return col === pinned && capPinned
      ? Math.min(preferred, Math.max(36, Math.floor(viewportWidth / 3)))
      : preferred;
  };
  return { pinned, ordered, visible, width, total: visible.reduce((sum, col) => sum + width(col), 0) };
}
