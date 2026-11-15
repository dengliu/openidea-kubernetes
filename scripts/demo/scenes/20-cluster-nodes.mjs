export default {
  id: "20-cluster-nodes",
  badge: "FREELENS-STYLE CLUSTER EXPLORER",
  title: "Multi-Cluster Hotbar, Cluster Overview & Live Node Metrics",
  subtitle: "Real-time CPU & Memory bars powered by metrics-server, plus Capacity, Conditions & scheduled Pods.",
  calloutPos: "bottom-left",
  voiceover:
    "Open the Kubernetes tool window to switch clusters from the hotbar, monitor live Node CPU and memory bars, and inspect node capacity, conditions, and scheduled pods.",
  preset: {
    hero: false,
    projectSidebar: true,
    explorerOpen: false,
    terminalOpen: false,
    openFile: "simple-app-kustomize/overlays/prod/patch-deployment.yaml",
  },
  async record(ctx) {
    const { page, k8sFrame } = ctx;

    // Close Project sidebar and open the Kubernetes Cluster Explorer
    const projectStripeBtn = page.locator('[data-stripe-button="project"]').first();
    if (await page.locator('[data-toolwindow="project"]').isVisible().catch(() => false)) {
      await ctx.clickLocator(projectStripeBtn, { moveMs: 340 });
      await page.waitForTimeout(180);
    }

    const k8sStripeBtn = page.locator('[data-stripe-button="plugin.dengliu.kubernetes.kubernetes"]');
    await ctx.clickLocator(k8sStripeBtn, { moveMs: 420 });

    await page.locator('iframe[data-plugin-webview="dengliu.kubernetes"]').waitFor({ state: "visible", timeout: 15_000 });
    await ctx.setAnchorSize("right", 1220);
    await k8sFrame.locator(".fl-nav-tree").waitFor({ state: "visible", timeout: 20_000 });
    await page.waitForTimeout(1800);
    await ctx.hold(1300);

    const nodesNav = k8sFrame.locator(".fl-tree-group-header", { hasText: "Nodes" }).first();
    await ctx.clickInFrame(nodesNav, { moveMs: 340 });
    const firstNodeRow = k8sFrame.locator(".fl-table tbody tr, .fl-table-row").first();
    await firstNodeRow.waitFor({ state: "visible", timeout: 15_000 });
    await page.waitForTimeout(900);
    await ctx.hold(1250);

    await ctx.clickInFrame(firstNodeRow, { offsetX: 110, moveMs: 320 });
    await page.waitForTimeout(1400);
    await ctx.hold(1200);

    await k8sFrame
      .locator(".fl-drawer-body, .fl-drawer")
      .first()
      .evaluate((el) => {
        const scroller = el.classList.contains("fl-drawer-body") ? el : el.querySelector(".fl-drawer-body") || el;
        scroller.scrollTo({ top: 540, behavior: "smooth" });
      })
      .catch(() => {});
    await page.waitForTimeout(550);
    await ctx.hold(1200);
  },
};
