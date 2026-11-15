export default {
  id: "15-helm-completion",
  badge: "HELM TEMPLATE INTELLIGENCE",
  title: "Helm Chart .Values Completion & Template-to-Values Navigation",
  subtitle:
    "Auto-completes .Values paths from simple-app-chart/values.yaml and jumps straight to value & _helpers.tpl definitions.",
  calloutPos: "bottom-right",
  voiceover:
    "Inside Helm templates, dot-Values paths auto-complete directly from your chart's values file, with instant navigation straight to the underlying value definition.",
  preset: {
    closeAllEditorTabs: true,
    hero: false,
    projectSidebar: true,
    explorerOpen: false,
    terminalOpen: false,
    openFile: "simple-app-chart/templates/deployment.yaml",
  },
  async record(ctx) {
    const { page } = ctx;
    const helmValuesLine = page.locator(".cm-line", { hasText: ".Values.image.repository" }).first();
    await ctx.clickLocator(helmValuesLine, { offsetX: 250, moveMs: 340 });
    await page.evaluate(() => {
      const text = window.__openideaPerf.editorText() || "";
      const target = ".Values.image.repository";
      const idx = text.indexOf(target);
      if (idx >= 0) window.__openideaPerf.setCursor(idx + ".Values.".length);
    });
    await page.evaluate(() => window.__openideaPerf.runAction("code.completion"));
    await page.locator(".cm-tooltip-autocomplete").waitFor({ state: "visible", timeout: 5000 }).catch(() => {});
    await ctx.hold(1350);
    await page.keyboard.press("Escape");
    await page.waitForTimeout(180);

    // Jump from `.Values.image.repository` directly to `simple-app-chart/values.yaml`
    await page.evaluate(() => {
      const text = window.__openideaPerf.editorText() || "";
      const target = ".Values.image.repository";
      const idx = text.indexOf(target);
      if (idx >= 0) window.__openideaPerf.setCursor(idx + ".Values.image.rep".length);
    });
    await ctx.hold(400);
    await page.evaluate(() => window.__openideaPerf.runAction("navigate.declaration"));
    await page
      .locator('[role="tab"][aria-selected="true"]', { hasText: "values.yaml" })
      .waitFor({ timeout: 10_000 })
      .catch(() => {});
    await ctx.hold(1100);
  },
};
