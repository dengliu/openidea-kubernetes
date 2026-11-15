export default {
  id: "18-kustomize-overlays",
  badge: "KUSTOMIZE OVERLAYS & PATCH INTELLIGENCE",
  title: "Kustomize Base & Patch Navigation with Patch-Aware Inspections",
  subtitle:
    "Jump from overlays/prod/kustomization.yaml directly to strategic-merge patches and base manifests with zero false-positive warnings.",
  calloutPos: "bottom-right",
  voiceover:
    "Working with Kustomize overlays? Jump straight from your production kustomization to strategic-merge patches and base manifests, backed by patch-aware inspections.",
  preset: {
    closeAllEditorTabs: true,
    hero: false,
    projectSidebar: true,
    explorerOpen: false,
    terminalOpen: false,
    openFile: "simple-app-kustomize/overlays/prod/kustomization.yaml",
  },
  async record(ctx) {
    const { page } = ctx;
    await ctx.hold(850);

    const patchLine = page.locator(".cm-line", { hasText: "patch-deployment.yaml" }).first();
    await ctx.clickLocator(patchLine, { offsetX: 170, moveMs: 340 });
    await page.evaluate(() => {
      const text = window.__openideaPerf.editorText() || "";
      const target = "patch-deployment.yaml";
      const idx = text.indexOf(target);
      if (idx >= 0) window.__openideaPerf.setCursor(idx + 6);
    });
    await ctx.hold(500);
    await page.evaluate(() => window.__openideaPerf.runAction("navigate.declaration"));
    await page
      .locator('[role="tab"][aria-selected="true"]', { hasText: "patch-deployment.yaml" })
      .waitFor({ timeout: 10_000 })
      .catch(() => {});
    await ctx.hold(1250);
  },
};
