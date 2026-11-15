export default {
  id: "50-debug-shell",
  badge: "POD & EPHEMERAL DEBUG SHELL IN TERMINAL",
  title: "Click Debug Shell to Attach & Open Live Terminal Below",
  subtitle:
    "Attach an ephemeral debugger container (dengliu/k8sdebugger) via kubectl debug --target=podinfo directly in OpenIDEA's native Terminal tab.",
  calloutPos: "top-left",
  voiceover:
    "To troubleshoot live workloads, attach an ephemeral debug container right into Open IDEA's native Terminal tab, sharing the pod's process and network namespace.",
  preset: {
    hero: false,
    projectSidebar: false,
    explorerOpen: true,
    rightWidth: 760,
    bottomHeight: 340,
    terminalOpen: false,
    drawerClosed: true,
    namespace: "default",
    explorerKind: "Pod",
  },
  async record(ctx) {
    const { page, k8sFrame } = ctx;

    const podinfoRow = k8sFrame.locator("tr, .fl-table-row", { hasText: "podinfo-" }).first();
    await ctx.clickInFrame(podinfoRow, { offsetX: 110, moveMs: 300 });
    await page.waitForTimeout(900);

    const prevTermCount = ctx.getSpawnedTermCount();
    await ctx.clickDrawerHeaderAction("Open Pod Shell in Terminal Tab");
    await ctx.hold(1300);

    const debugShellCustomItem = k8sFrame
      .locator(".fl-container-menu .fl-container-menu-item", { hasText: /Debug Shell \(dengliu\/k8sdebugger/ })
      .first();
    await ctx.clickInFrame(debugShellCustomItem, { moveMs: 340 });

    await page.locator('[data-toolwindow="terminal"]').waitFor({ state: "visible", timeout: 25_000 });
    const dbgDeadline = Date.now() + 20_000;
    while (ctx.getSpawnedTermCount() <= prevTermCount && Date.now() < dbgDeadline) {
      await page.waitForTimeout(200);
    }
    await page.waitForTimeout(1600);
    await ctx.hold(950);

    const termPane = page.locator('[data-toolwindow="terminal"]').first();
    const termBox = await termPane.boundingBox();
    if (termBox) {
      await ctx.moveCursorTo(termBox.x + 380, termBox.y + 130, 320);
      await page.evaluate(([x, y]) => window.__demoUi.ripple(x, y), [termBox.x + 380, termBox.y + 130]);
    }

    await ctx.typeInActiveTerminal("ps aux && uname -sr");
    await ctx.typeInActiveTerminal("curl -s http://127.0.0.1:9898/version && echo");
  },
  async cleanup(ctx) {
    ctx.cleanDebugWatcherPods();
  },
};
