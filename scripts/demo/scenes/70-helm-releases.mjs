export default {
  id: "70-helm-releases",
  badge: "BROWSE HELM RELEASES",
  title: "Helm Releases Across Namespaces & Release Inspection Drawer",
  subtitle: "Inspect Redis, ScyllaDB, Ingress-NGINX, Node Exporter & Podinfo releases — Notes, Values, Pods & Resources.",
  calloutPos: "bottom-left",
  voiceover:
    "Under Helm Releases, browse deployed charts like Redis, Scylla D B, Ingress NGINX, and Podinfo — inspecting release notes, custom values, and managed resources.",
  preset: {
    hero: false,
    projectSidebar: false,
    closeAllEditorTabs: true,
    explorerOpen: true,
    rightWidth: 1300,
    terminalOpen: false,
    drawerClosed: true,
    namespace: "all",
    explorerKind: "HelmRelease",
  },
  async record(ctx) {
    const { page, k8sFrame } = ctx;
    await ctx.hold(1300);

    const redisHelmRow = k8sFrame.locator("tr, .fl-table-row", { hasText: /\bredis\b/ }).first();
    if (await redisHelmRow.isVisible().catch(() => false)) {
      await ctx.clickInFrame(redisHelmRow, { offsetX: 110, moveMs: 300 });
      await page.waitForTimeout(1400);
      await ctx.hold(1250);

      await k8sFrame
        .locator(".fl-drawer-body, .fl-drawer")
        .first()
        .evaluate((el) => {
          const scroller = el.classList.contains("fl-drawer-body") ? el : el.querySelector(".fl-drawer-body") || el;
          scroller.scrollTo({ top: 480, behavior: "smooth" });
        })
        .catch(() => {});
      await page.waitForTimeout(550);
      await ctx.hold(1200);
    }

    const scyllaHelmRow = k8sFrame.locator("tr, .fl-table-row", { hasText: "scylladb" }).first();
    if (await scyllaHelmRow.isVisible().catch(() => false)) {
      await ctx.clickInFrame(scyllaHelmRow, { offsetX: 110, moveMs: 300 });
      await page.waitForTimeout(1400);
      await ctx.hold(1250);

      await k8sFrame
        .locator(".fl-drawer-body, .fl-drawer")
        .first()
        .evaluate((el) => {
          const scroller = el.classList.contains("fl-drawer-body") ? el : el.querySelector(".fl-drawer-body") || el;
          scroller.scrollTo({ top: 460, behavior: "smooth" });
        })
        .catch(() => {});
      await page.waitForTimeout(550);
      await ctx.hold(1200);
    }
  },
};
