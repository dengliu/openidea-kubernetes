export default {
  id: "10-yaml-intelligence",
  badge: "YAML MANIFEST INTELLIGENCE",
  title: "Manifest Inspections, 1-Click Quick Fixes & Cross-Resource Navigation",
  subtitle:
    "Real-time lint checks (:latest tag, missing resources), gutter markers linking Service ↔ Deployment ↔ HPA ↔ Secret, and Go to Definition.",
  calloutPos: "bottom-right",
  voiceover:
    "As you edit plain manifests, real-time inspections catch mutable latest tags with one-click quick fixes, while cross-resource navigation jumps straight to linked Secrets and Services.",
  preset: {
    resetManifests: true,
    hero: false,
    projectSidebar: true,
    explorerOpen: false,
    terminalOpen: false,
    openFile: "plain-manifest/deployment.yaml",
  },
  async record(ctx) {
    const { page } = ctx;
    const imageLine = page.locator(".cm-line", { hasText: "image: nginx:" }).first();
    await ctx.clickLocator(imageLine, { offsetX: 210, moveMs: 380 });

    // Edit `nginx:1.27-alpine` -> `nginx:latest` via PerfProbe to trigger the live `:latest` inspection
    await page.evaluate(async () => {
      const text = window.__openideaPerf.editorText() || "";
      const next = text.replace("nginx:1.27-alpine", "nginx:latest");
      if (next !== text) {
        window.__openideaPerf.setText(next);
        const idx = next.indexOf("nginx:latest");
        if (idx >= 0) window.__openideaPerf.setCursor(idx + "nginx:la".length);
      }
      await window.__openideaPerf.runAction("file.saveAll").catch(() => {});
    });
    await page.waitForTimeout(450);
    await ctx.hold(600);

    const bulb = page.locator('[title^="Show intentions"]').first();
    if (await bulb.isVisible().catch(() => false)) {
      const bBox = await bulb.boundingBox();
      if (bBox) await ctx.moveCursorTo(bBox.x + bBox.width / 2, bBox.y + bBox.height / 2, 250);
    }
    await page.evaluate(() => window.__openideaPerf.runAction("code.showIntentions"));
    await page.waitForTimeout(400);
    const k8sFix = page.getByRole("option", { name: /Pin image|Kubernetes:/i }).first();
    const targetFix = (await k8sFix.isVisible({ timeout: 3500 }).catch(() => false))
      ? k8sFix
      : page.getByRole("option").first();
    if (await targetFix.isVisible({ timeout: 1500 }).catch(() => false)) {
      const fixBox = await targetFix.boundingBox();
      if (fixBox) {
        await ctx.moveCursorTo(fixBox.x + Math.min(180, fixBox.width / 2), fixBox.y + fixBox.height / 2, 260);
        await targetFix.hover().catch(() => {});
      }
      await ctx.hold(1350);
      await targetFix.click();
      await page.evaluate(() => window.__openideaPerf.runAction("file.saveAll").catch(() => {}));
      await ctx.hold(900);
    } else {
      await page.keyboard.press("Escape");
    }

    const secretRefLine = page.locator(".cm-line", { hasText: "name: simple-app-secret" }).first();
    await ctx.clickLocator(secretRefLine, { offsetX: 220, moveMs: 340 });
    await page.evaluate(() => {
      const text = window.__openideaPerf.editorText() || "";
      const idx = text.indexOf("simple-app-secret");
      if (idx >= 0) window.__openideaPerf.setCursor(idx + 6);
    });
    await ctx.hold(450);
    await page.evaluate(() => window.__openideaPerf.runAction("navigate.declaration"));
    await page.waitForTimeout(400);

    // If multiple manifests define `simple-app-secret` (plain-manifest + kustomize base), double-click the plain-manifest target
    if (!(await page.locator('[role="tab"][aria-selected="true"]', { hasText: "secret.yaml" }).isVisible().catch(() => false))) {
      await ctx.hold(650);
      const usageRow = page.getByText("plain-manifest").last();
      if (await usageRow.isVisible().catch(() => false)) {
        const uBox = await usageRow.boundingBox();
        if (uBox) {
          await ctx.moveCursorTo(uBox.x + 40, uBox.y + uBox.height / 2, 260);
          await page.mouse.dblclick(uBox.x + 40, uBox.y + uBox.height / 2);
        }
      }
    }
    await page
      .locator('[role="tab"][aria-selected="true"]', { hasText: "secret.yaml" })
      .waitFor({ timeout: 5_000 })
      .catch(async () => {
        await ctx.openWsFile("plain-manifest/secret.yaml");
      });
    await ctx.hold(1100);
  },
};
