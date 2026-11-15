export default {
  id: "60-deploy-spec-apply",
  badge: "LIVE DEPLOYMENT SPEC EDITOR & APPLY",
  title: "Edit Replicas, Diff & Apply in Full-Width Native Editor Tab",
  subtitle: "With Project window closed, Reload, Diff, and Save & Apply sit side-by-side — edit spec.replicas: 1 → 3 and apply live.",
  calloutPos: "bottom-right",
  voiceover:
    "Open any live Deployment spec in a full-width native editor tab, update the replica count, preview changes in the side-by-side Diff viewer, and click Save and Apply to scale live.",
  preset: {
    hero: false,
    projectSidebar: false,
    closeAllEditorTabs: true,
    explorerOpen: true,
    rightWidth: 760,
    terminalOpen: false,
    drawerClosed: true,
    namespace: "default",
    explorerKind: "Deployment",
  },
  async record(ctx) {
    const { page, k8sFrame } = ctx;
    // Keep manual Save & Apply explicit while switching between the YAML editor and Diff viewer
    await page.evaluate(async () => {
      const projMod = await import("/src/platform/project.ts");
      projMod.useProject.setState((s) => ({
        settings: {
          ...(s.settings || {}),
          files: { ...(s.settings?.files || {}), autoSave: false },
        },
      }));
    });
    ctx.scaleDeployment("podinfo", "default", 1, true);
    await page.waitForTimeout(1500);

    const podinfoDeployRow = k8sFrame.locator("tr, .fl-table-row", { hasText: "podinfo" }).first();
    await ctx.clickInFrame(podinfoDeployRow, { offsetX: 110, moveMs: 300 });
    await page.waitForTimeout(850);

    await ctx.clickDrawerHeaderAction("Edit Live YAML in Editor Tab");
    await page.locator('[role="tab"][aria-selected="true"]', { hasText: /Deployment:.*podinfo/ }).waitFor({ state: "visible", timeout: 12_000 });
    await ctx.closeExplorerDrawer();
    await page.waitForTimeout(650);

    const iframeEl = page.locator('iframe[data-plugin-webview="dengliu.kubernetes"]');
    const diffBtn = page.getByRole("button", { name: /^Diff$/ }).first();
    const saveApplyBtn = page.getByRole("button", { name: /Save & Apply/ }).first();
    const [saveBox, iframeBoxNow] = await Promise.all([saveApplyBtn.boundingBox(), iframeEl.boundingBox()]);
    if (!saveBox || !iframeBoxNow || saveBox.x + saveBox.width > iframeBoxNow.x - 4) {
      throw new Error(`Save & Apply button clipped! saveRight=${saveBox && saveBox.x + saveBox.width}, explorerLeft=${iframeBoxNow?.x}`);
    }

    const deployCmContent = page.locator('.cm-content[aria-label*="podinfo"]').first();
    await deployCmContent.waitFor({ state: "visible", timeout: 10_000 });

    await page.evaluate(() => {
      const cmDom = document.querySelector('.cm-content[aria-label*="podinfo"]');
      const lines = Array.from(cmDom?.querySelectorAll(".cm-line") || []);
      const repLine = lines.find((l) => /^\s*replicas:\s*\d+/.test(l.textContent || ""));
      if (repLine) repLine.scrollIntoView({ block: "center" });
      const scroller = cmDom?.closest(".cm-scroller");
      if (scroller) scroller.scrollLeft = 0;
    });
    await page.waitForTimeout(350);
    await ctx.hold(700);

    const replicasLine = deployCmContent.locator(".cm-line").filter({ hasText: /^\s*replicas:\s*1\b/ }).first();
    await ctx.clickLocator(replicasLine, { offsetX: 110, moveMs: 320 });
    await page.keyboard.press("Meta+ArrowRight");
    await page.waitForTimeout(100);
    await page.keyboard.press("Backspace");
    await ctx.snap(140);
    await page.keyboard.type("3");
    await page.waitForTimeout(280);

    if (!(await page.getByText("• Modified").isVisible().catch(() => false))) {
      await page.keyboard.press("Meta+ArrowRight");
      await page.keyboard.press("Backspace");
      await page.keyboard.type("3");
      await page.waitForTimeout(250);
    }
    await page.evaluate(() => {
      const scroller = document.querySelector('.cm-content[aria-label*="podinfo"]')?.closest(".cm-scroller");
      if (scroller) scroller.scrollLeft = 0;
    });
    await ctx.hold(1050);

    await ctx.clickLocator(diffBtn, { moveMs: 300 });
    await page.locator('[role="tab"][aria-selected="true"]', { hasText: /Diff:.*podinfo/ }).waitFor({ state: "visible", timeout: 10_000 }).catch(() => {});
    await page.waitForTimeout(750);
    await ctx.hold(1600);

    const deployEditorTab = page.locator('[role="tab"]', { hasText: /Deployment:.*podinfo/ }).first();
    await ctx.clickLocator(deployEditorTab, { moveMs: 280 });
    await page.waitForTimeout(350);

    await ctx.clickLocator(saveApplyBtn, { moveMs: 320 });
    await page.waitForTimeout(2100);
    await page.evaluate(() => {
      const scroller = document.querySelector('.cm-content[aria-label*="podinfo"]')?.closest(".cm-scroller");
      if (scroller) scroller.scrollLeft = 0;
    });
    await ctx.hold(1350);

    const topPodsTab = k8sFrame.locator(".fl-top-tab", { hasText: /^Pods$/ }).first();
    if (await topPodsTab.isVisible().catch(() => false)) {
      await ctx.clickInFrame(topPodsTab, { moveMs: 280 });
      await page.waitForTimeout(1150);
      await ctx.hold(1200);
    }
  },
  async cleanup(ctx) {
    await ctx.page
      .evaluate(async () => {
        const projMod = await import("/src/platform/project.ts");
        projMod.useProject.setState((s) => ({
          settings: {
            ...(s.settings || {}),
            files: { ...(s.settings?.files || {}), autoSave: true },
          },
        }));
      })
      .catch(() => {});
    ctx.scaleDeployment("podinfo", "default", 1);
  },
};
