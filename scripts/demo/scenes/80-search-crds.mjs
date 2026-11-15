export default {
  id: "80-search-crds",
  badge: "CLUSTER SEARCH & CUSTOM RESOURCES",
  title: "80k+ Resource Search & CRDs Grouped by API Group",
  subtitle: "Instant watch-cache Table search over loopback kubectl proxy, plus dynamic CRD API group navigation.",
  calloutPos: "bottom-left",
  voiceover:
    "Search across tens of thousands of cluster resources in seconds over a local loopback proxy, and explore Custom Resource Definitions grouped cleanly by API group.",
  preset: {
    hero: false,
    projectSidebar: false,
    closeAllEditorTabs: true,
    explorerOpen: true,
    rightWidth: 1300,
    terminalOpen: false,
    drawerClosed: true,
    namespace: "all",
    explorerKind: "Pod",
  },
  async record(ctx) {
    const { page, k8sFrame } = ctx;

    const searchInput = k8sFrame.locator(".fl-search-box").first();
    await ctx.clickInFrame(searchInput, { moveMs: 300 });
    await searchInput.focus();
    for (const ch of "coredns") {
      await searchInput.pressSequentially(ch, { delay: 35 });
      await ctx.snap(75);
    }
    await page.waitForTimeout(650);
    await ctx.hold(1250);

    const clearSearchBtn = k8sFrame.locator(".fl-search-clear-btn").first();
    if (await clearSearchBtn.isVisible().catch(() => false)) {
      await ctx.clickInFrame(clearSearchBtn, { moveMs: 220 });
    }

    const crdHeader = k8sFrame.locator(".fl-tree-group-header", { hasText: "Custom Resources" }).first();
    await ctx.clickInFrame(crdHeader, { moveMs: 300 });
    const definitionsChild = k8sFrame.locator(".fl-tree-child", { hasText: "Definitions" }).first();
    if (await definitionsChild.isVisible().catch(() => false)) {
      await ctx.clickInFrame(definitionsChild, { moveMs: 240 });
    }
    await page.waitForTimeout(1050);
    await ctx.hold(850);

    const smDefRow = k8sFrame
      .locator(".fl-table tbody tr, .fl-table-row", { hasText: /servicemonitors.*monitoring\.coreos\.com/ })
      .first();
    if (await smDefRow.isVisible().catch(() => false)) {
      await ctx.clickInFrame(smDefRow, { offsetX: 110, moveMs: 280 });
      await page.waitForTimeout(950);
      await ctx.hold(850);
    }

    const simpleAppSmRow = k8sFrame.locator(".fl-table tbody tr, .fl-table-row", { hasText: "simple-app" }).first();
    if (await simpleAppSmRow.isVisible().catch(() => false)) {
      await ctx.clickInFrame(simpleAppSmRow, { offsetX: 110, moveMs: 280 });
      await page.waitForTimeout(950);
    }
    await ctx.hold(1250);
  },
};
