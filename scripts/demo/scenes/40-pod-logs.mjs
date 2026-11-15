import { spawn } from "node:child_process";

export default {
  id: "40-pod-logs",
  badge: "FOLLOW POD LOGS IN NATIVE EDITOR TAB",
  title: "Real-Time Streaming Pod Logs in CodeMirror 6 Editor Tab",
  subtitle: "Follow live container stdout/stderr (--follow) in a wide native OpenIDEA editor tab with Timestamps, Previous, Soft-wrap & Find.",
  calloutPos: "bottom-right",
  voiceover:
    "Click Pod Logs to follow real-time container output in a native editor tab, complete with live timestamps and instant log search.",
  preset: {
    hero: false,
    projectSidebar: false,
    closeAllEditorTabs: true,
    explorerOpen: true,
    rightWidth: 760,
    terminalOpen: false,
    drawerClosed: true,
    namespace: "default",
    explorerKind: "Pod",
  },
  async record(ctx) {
    const { page, k8sFrame, kubeContext, multiPodName } = ctx;

    const multiPodRow = k8sFrame.locator("tr, .fl-table-row", { hasText: "multi-container-demo-" }).first();
    await ctx.clickInFrame(multiPodRow, { offsetX: 110, moveMs: 300 });
    await page.waitForTimeout(850);

    await ctx.clickDrawerHeaderAction("Pod Logs");
    const webAppMenuItem = k8sFrame.locator(".fl-container-menu .fl-container-menu-item", { hasText: "web-app" }).first();
    if (await webAppMenuItem.isVisible({ timeout: 2500 }).catch(() => false)) {
      await ctx.clickInFrame(webAppMenuItem, { moveMs: 240 });
    }

    await page.locator('[role="tab"]', { hasText: /Logs: multi-container-demo/ }).first().waitFor({ state: "visible", timeout: 10_000 });
    await page.waitForTimeout(900);
    await ctx.hold(700);

    if (multiPodName) {
      for (const reqPath of ["/?service=catalog", "/?service=cart", "/?service=checkout", "/?probe=readyz"]) {
        spawn("/usr/local/bin/kubectl", [
          "--context",
          kubeContext,
          "exec",
          "-n",
          "default",
          multiPodName,
          "-c",
          "web-app",
          "--",
          "wget",
          "-qO-",
          `http://127.0.0.1:80${reqPath}`,
        ]);
        await page.waitForTimeout(340);
        await ctx.snap(340);
      }
    }

    const timestampsToggle = page.getByLabel("Timestamps").first();
    if (await timestampsToggle.isVisible().catch(() => false)) {
      await ctx.clickLocator(timestampsToggle, { moveMs: 300 });
      await page.waitForTimeout(850);
      await ctx.hold(900);
    }

    const findInLogsBtn = page.locator('button[title="Find in Logs"]').first();
    if (await findInLogsBtn.isVisible().catch(() => false)) {
      await ctx.clickLocator(findInLogsBtn, { moveMs: 260 });
      await page.waitForTimeout(220);
      await page.keyboard.type("checkout", { delay: 40 });
      await page.waitForTimeout(350);
      await ctx.hold(1100);
      await page.keyboard.press("Escape");
    }
  },
};
