export default {
  id: "00-intro",
  badge: "KUBERNETES FOR OPENIDEA",
  title: "Workspace Intelligence + Live Cluster Operations",
  subtitle: "Custom Kubernetes, Helm & Kustomize file and folder icons, cross-resource gutter markers, and live cluster status bar.",
  calloutPos: "bottom-right",
  voiceover:
    "Meet Kubernetes for Open IDEA — bringing a Freelens-style cluster explorer, live container shells, and JetBrains-grade YAML, Helm, and Kustomize intelligence together in one native I D E.",
  voiceOffsetMs: 200,
  voiceTailPadMs: 450,
  preset: {
    resetManifests: true,
    hero: true,
    projectSidebar: true,
    explorerOpen: false,
    terminalOpen: false,
    openFile: "plain-manifest/deployment.yaml",
  },
  async record(ctx) {
    // Hold Hero title card (2.2s in silent cut, 3.6s in narrated cut)
    await ctx.snap(2200, 3600);

    // Reveal the live kube-app workspace (plain-manifest + simple-app-chart + simple-app-kustomize)
    await ctx.setHero(false);
    await ctx.showSceneCallout();

    const chartTreeItem = ctx.page.getByRole("treeitem", { name: /^Chart\.yaml\b/ }).first();
    if (await chartTreeItem.isVisible().catch(() => false)) {
      const cb = await chartTreeItem.boundingBox();
      if (cb) await ctx.moveCursorTo(cb.x + 80, cb.y + cb.height / 2, 550);
    }
    await ctx.hold(650);
    const kustPatchItem = ctx.page.getByRole("treeitem", { name: /^patch-deployment\.yaml\b/ }).last();
    if (await kustPatchItem.isVisible().catch(() => false)) {
      const kb = await kustPatchItem.boundingBox();
      if (kb) await ctx.moveCursorTo(kb.x + 95, kb.y + kb.height / 2, 550);
    }
    await ctx.hold(650);
  },
};
