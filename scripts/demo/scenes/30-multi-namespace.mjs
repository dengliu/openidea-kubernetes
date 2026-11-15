export default {
  id: "30-multi-namespace",
  badge: "MULTI-NAMESPACE LIVE WATCH",
  title: "Virtualized Multi-Namespace Picker & Zero-Button Auto-Scroll",
  subtitle: "Select multiple namespaces simultaneously with removable chips, parallel kubectl fan-out, and sub-second watch=true deltas.",
  calloutPos: "bottom-left",
  voiceover:
    "Need to watch several services at once? The virtualized multi-namespace picker streams live watch updates across multiple namespaces simultaneously.",
  preset: {
    hero: false,
    projectSidebar: false,
    explorerOpen: true,
    rightWidth: 1220,
    terminalOpen: false,
    drawerClosed: true,
    namespace: "all",
    explorerKind: "Pod",
  },
  async record(ctx) {
    const { page, k8sFrame } = ctx;
    const nsPickerBtn = k8sFrame.locator(".fl-ns-picker-btn").first();
    await ctx.clickInFrame(nsPickerBtn, { moveMs: 320 });
    await k8sFrame.locator(".fl-ns-popover").waitFor({ state: "visible", timeout: 10_000 });
    await ctx.hold(550);

    for (const nsName of ["default", "cache", "scylla", "monitoring"]) {
      const opt = k8sFrame
        .locator(".fl-ns-list .fl-ns-opt")
        .filter({ has: k8sFrame.locator(".fl-ns-opt-label", { hasText: new RegExp(`^${nsName}$`) }) })
        .first();
      if (!(await opt.isVisible().catch(() => false))) continue;
      const isChecked = await opt.locator(".fl-ns-check.checked").count().catch(() => 0);
      if (!isChecked) {
        await ctx.clickInFrame(opt, { offsetX: 90, moveMs: 260 });
        await page.waitForTimeout(280);
      }
    }
    await ctx.hold(1250);
    await ctx.clickInFrame(nsPickerBtn, { moveMs: 240 });
    await page.waitForTimeout(800);
    await ctx.hold(1150);
  },
};
