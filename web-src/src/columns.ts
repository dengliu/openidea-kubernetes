import type { ColumnDef } from "./types";

export interface ColumnLayout {
  order?: string[];
  visible?: Record<string, boolean>;
  widths?: Record<string, number>;
}

export function tableColumns(defs: ColumnDef[], layout: ColumnLayout, viewportWidth: number) {
  const pinned = defs.find(c => c.id === "name" || c.id === "crd_resource") ?? defs[0];
  const remaining = new Map(defs.map(c => [c.id, c]));
  const ordered: ColumnDef[] = [];
  for (const id of [pinned.id, ...(layout.order ?? [])]) {
    const col = remaining.get(id);
    if (col) {
      ordered.push(col);
      remaining.delete(id);
    }
  }
  ordered.push(...remaining.values());
  const visible = ordered.filter(col => col === pinned || (layout.visible?.[col.id] ?? col.defaultVisible));
  const width = (col: ColumnDef) => {
    const preferred = Math.max(36, layout.widths?.[col.id] ?? col.defaultWidth);
    return col === pinned && viewportWidth > 0
      ? Math.min(preferred, Math.max(36, Math.floor(viewportWidth / 3)))
      : preferred;
  };
  return { pinned, ordered, visible, width, total: visible.reduce((sum, col) => sum + width(col), 0) };
}
