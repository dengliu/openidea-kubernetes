export default {
  id: "99-outro",
  badge: "",
  title: "",
  subtitle: "",
  calloutPos: "bottom-left",
  voiceover: "Install Kubernetes for Open IDEA today from the plugin marketplace.",
  voiceOffsetMs: 200,
  voiceTailPadMs: 600,
  preset: {
    hero: false,
    projectSidebar: false,
    explorerOpen: true,
    rightWidth: 1300,
    terminalOpen: false,
  },
  async record(ctx) {
    await ctx.setCallout("", "", "");
    await ctx.setHero(
      true,
      "Install Kubernetes for OpenIDEA Today",
      `Settings › Plugins › Available › Search “Kubernetes” (dengliu.kubernetes v${ctx.pluginVersion})`,
      ["Apache-2.0 Open Source", "Signed Publisher Package", "github.com/dengliu/openidea-kubernetes"],
    );
    await ctx.snap(2600, 2600);
  },
};
