/**
 * Reusable Playwright + `openidea-server --stdio` capture harness with
 * declarative `applyPreset(preset)` state normalization so any scene (or subset
 * of scenes) can be recorded in complete isolation without affecting other scenes.
 */
import { execFileSync, spawn } from "node:child_process";
import { cpSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import http from "node:http";
import { homedir, tmpdir } from "node:os";
import { basename, extname, join, resolve, sep } from "node:path";
import { createRequire } from "node:module";

export async function captureScenes({
  scenesToRecord,
  pluginRoot,
  openideaRoot,
  kubeAppRoot,
  kubeContext,
  workDir,
  sceneCacheRoot,
}) {
  if (!scenesToRecord.length) return;
  const resolvedKubeAppRoot = kubeAppRoot || join(import.meta.dirname, "fixtures/kube-app");
  if (!existsSync(resolvedKubeAppRoot)) {
    throw new Error(`Demo workspace directory not found: ${resolvedKubeAppRoot}`);
  }

  const pluginTomlText = readFileSync(join(pluginRoot, "plugin.toml"), "utf8");
  const pluginVersion = (/^version\s*=\s*"([^"]+)"/m.exec(pluginTomlText)?.[1] || "0.3.6").trim();

  const requireFromOpenidea = createRequire(join(openideaRoot, "package.json"));
  const { chromium } = requireFromOpenidea("@playwright/test");

  function runKubectl(args) {
    return execFileSync("/usr/local/bin/kubectl", ["--context", kubeContext, ...args], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
    }).trim();
  }

  function scaleDeployment(name, ns, replicas, waitRollout = false) {
    try {
      runKubectl(["scale", `deployment/${name}`, "-n", ns, `--replicas=${replicas}`]);
      if (waitRollout) {
        runKubectl(["rollout", "status", `deployment/${name}`, "-n", ns, "--timeout=45s"]);
        const deadline = Date.now() + 15_000;
        while (Date.now() < deadline) {
          const cur = runKubectl(["get", `deployment/${name}`, "-n", ns, "-o", "jsonpath={.status.replicas}"]);
          if (String(cur).trim() === String(replicas)) break;
          Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 350);
        }
      }
    } catch {}
  }

  function cleanDebugWatcherPods() {
    try {
      const watchPods = runKubectl([
        "get",
        "pods",
        "-n",
        "default",
        "-l",
        "app.kubernetes.io/managed-by=openidea-dbg-watch",
        "-o",
        "name",
      ]);
      if (watchPods) {
        for (const wp of watchPods.split("\n").filter(Boolean)) {
          runKubectl(["delete", wp, "-n", "default", "--force", "--grace-period=0"]);
        }
      }
    } catch {}
  }

  scaleDeployment("podinfo", "default", 1);

  let multiPodName = "";
  try {
    multiPodName = runKubectl([
      "get",
      "pods",
      "-n",
      "default",
      "-l",
      "app=multi-container-demo",
      "-o",
      "jsonpath={.items[0].metadata.name}",
    ]);
  } catch {}

  const serverHome = join(workDir, "server-home");
  const projectRoot = join(workDir, "kube-app");
  rmSync(workDir, { recursive: true, force: true });
  mkdirSync(join(serverHome, "data/remote-plugins"), { recursive: true });

  writeFileSync(join(serverHome, "data/remote-plugins/links.json"), JSON.stringify({ paths: [pluginRoot] }, null, 2));

  const appSupportGrantsPath = join(homedir(), "Library/Application Support/ai.l0ng.openidea/plugins/grants.json");
  let binariesBlock = {};
  if (existsSync(appSupportGrantsPath)) {
    const fullGrants = JSON.parse(readFileSync(appSupportGrantsPath, "utf8"));
    for (const [, g] of fullGrants.grants || []) {
      if (g.pluginId === "dengliu.kubernetes" && g.binaries && Object.keys(g.binaries).length > 0) {
        binariesBlock = g.binaries;
        break;
      }
    }
  }

  writeFileSync(
    join(serverHome, "data/remote-plugins/grants.json"),
    JSON.stringify(
      {
        format: 1,
        migrated: true,
        grants: [
          [
            { kind: "path", value: pluginRoot },
            {
              pluginId: "dengliu.kubernetes",
              version: pluginVersion,
              packageHash: "",
              confirmed: true,
              granted: [
                "clipboard",
                "editor.read",
                "editor.write",
                "events",
                "net:127.0.0.1",
                "process:helm",
                "process:kubectl",
                "process:kustomize",
                "project.read",
                "storage",
              ],
              netHosts: [],
              declined: [],
              binaries: binariesBlock,
              projectBinaries: {},
              programChoices: {},
              askOnUse: false,
            },
          ],
        ],
      },
      null,
      2,
    ),
  );

  function writeCleanManifests() {
    rmSync(projectRoot, { recursive: true, force: true });
    cpSync(resolvedKubeAppRoot, projectRoot, {
      recursive: true,
      force: true,
      filter: (src) => {
        const name = basename(src);
        return name !== ".git" && name !== ".idea" && name !== ".DS_Store";
      },
    });
  }

  writeCleanManifests();

  // Serve plugin webview assets with frame-bridge.js injected
  const bridgeSource = readFileSync(join(openideaRoot, "src/features/plugins/views/frame-bridge.js"), "utf8")
    .trim()
    .replace(/;$/, "");

  const MIME_BY_EXT = {
    ".html": "text/html; charset=utf-8",
    ".js": "text/javascript; charset=utf-8",
    ".css": "text/css; charset=utf-8",
    ".svg": "image/svg+xml",
    ".json": "application/json; charset=utf-8",
    ".woff2": "font/woff2",
  };

  const pluginWebDir = join(pluginRoot, "web");
  const webStaticServer = http.createServer((req, res) => {
    const urlPath = decodeURIComponent((req.url || "/").split("?")[0]).replace(/^\/+/, "");
    const rel = urlPath.startsWith("web/") ? urlPath.slice(4) : urlPath || "index.html";
    const filePath = resolve(pluginWebDir, rel);
    if (!filePath.startsWith(pluginWebDir) || !existsSync(filePath)) {
      res.writeHead(404, { "Access-Control-Allow-Origin": "*" });
      res.end("Not found");
      return;
    }
    const ext = extname(filePath);
    const mime = MIME_BY_EXT[ext] || "application/octet-stream";
    let body = readFileSync(filePath);
    if (ext === ".html") {
      const html = body.toString("utf8");
      const bridgeTag = `<script>(${bridgeSource})(null);</script>`;
      const headIdx = html.toLowerCase().indexOf("<head>");
      const injected =
        headIdx >= 0 ? html.slice(0, headIdx + 6) + bridgeTag + html.slice(headIdx + 6) : bridgeTag + html;
      body = Buffer.from(injected, "utf8");
    }
    res.writeHead(200, { "Content-Type": mime, "Access-Control-Allow-Origin": "*", "Cache-Control": "no-cache" });
    res.end(body);
  });

  await new Promise((r) => webStaticServer.listen(0, "127.0.0.1", r));
  const webStaticPort = webStaticServer.address().port;

  // Dynamically parse SVG assets & fileIcons from plugin.toml
  const readSvgSafe = (rel) =>
    rel && existsSync(join(pluginRoot, rel)) ? readFileSync(join(pluginRoot, rel), "utf8") : "";
  const topIconPath = /^icon\s*=\s*"([^"]+)"/m.exec(pluginTomlText)?.[1] || "icons/kubernetes-logo.svg";
  const pluginLogoSvg = readSvgSafe(topIconPath);
  const viewIconSvg = readSvgSafe("icons/kubernetes.svg") || pluginLogoSvg;
  const settingsSchema = JSON.parse(readFileSync(join(pluginRoot, "settings.schema.json"), "utf8"));

  const FILE_ICONS = [];
  const ASSET_ENTRIES = [["viewIcon/kubernetes", viewIconSvg]];
  for (const block of pluginTomlText.split("[[contributes.fileIcons]]").slice(1)) {
    const body = block.split(/\n\[\[/)[0] || "";
    const id = /^id\s*=\s*"([^"]+)"/m.exec(body)?.[1];
    const iconRel = /^icon\s*=\s*"([^"]+)"/m.exec(body)?.[1];
    const iconDarkRel = /^iconDark\s*=\s*"([^"]+)"/m.exec(body)?.[1];
    const patMatch = /^patterns\s*=\s*(\[[^\]]*\])/m.exec(body);
    const patterns = patMatch ? JSON.parse(patMatch[1]) : [];
    if (!id || !iconRel) continue;
    const lightSvg = readSvgSafe(iconRel);
    const darkSvg = iconDarkRel ? readSvgSafe(iconDarkRel) : "";
    if (lightSvg) ASSET_ENTRIES.push([`fileIcon/${id}/light`, lightSvg]);
    if (darkSvg) ASSET_ENTRIES.push([`fileIcon/${id}/dark`, darkSvg]);
    FILE_ICONS.push({ id, patterns, dark: Boolean(darkSvg) });
  }

  const COMMANDS = [
    { id: "refresh", title: "Kubernetes: Refresh Cluster State", category: "Kubernetes" },
    { id: "switch-context", title: "Kubernetes: Switch Context", category: "Kubernetes" },
    { id: "switch-namespace", title: "Kubernetes: Switch Namespace", category: "Kubernetes" },
    { id: "apply", title: "Kubernetes: Apply Current Manifest", category: "Kubernetes", when: "editorLang == yaml" },
    { id: "diff", title: "Kubernetes: Diff Current Manifest with Cluster", category: "Kubernetes", when: "editorLang == yaml" },
    { id: "delete-manifest", title: "Kubernetes: Delete Resources in Current Manifest", category: "Kubernetes", when: "editorLang == yaml" },
    { id: "dry-run", title: "Kubernetes: Validate Current Manifest (Dry Run)", category: "Kubernetes", when: "editorLang == yaml" },
    { id: "inspect", title: "Kubernetes: Inspect Current File", category: "Kubernetes", when: "editorLang == yaml" },
    { id: "scan-project", title: "Kubernetes: Inspect All Project Manifests", category: "Kubernetes" },
    { id: "follow-logs", title: "Kubernetes: Follow Pod Logs in Terminal", category: "Kubernetes" },
    { id: "exec-shell", title: "Kubernetes: Open Shell in Pod Container", category: "Kubernetes" },
    { id: "port-forward", title: "Kubernetes: Port Forward to Resource", category: "Kubernetes" },
    { id: "scale", title: "Kubernetes: Scale Workload Replicas", category: "Kubernetes" },
    { id: "rollout-restart", title: "Kubernetes: Rollout Restart Workload", category: "Kubernetes" },
    { id: "helm-template", title: "Kubernetes: Render Helm Chart Templates", category: "Kubernetes" },
    { id: "kustomize-build", title: "Kubernetes: Build Kustomize Overlay", category: "Kubernetes" },
  ];

  const MENUS = [
    { menu: "editor.context", command: "apply", when: "editorLang == yaml" },
    { menu: "editor.context", command: "diff", when: "editorLang == yaml" },
    { menu: "editor.context", command: "dry-run", when: "editorLang == yaml" },
    { menu: "editor.context", command: "delete-manifest", when: "editorLang == yaml" },
    { menu: "editor.context", command: "inspect", when: "editorLang == yaml" },
    { menu: "tools", command: "refresh" },
    { menu: "tools", command: "switch-context" },
    { menu: "tools", command: "switch-namespace" },
    { menu: "tools", command: "scan-project" },
  ];

  const PLUGIN_SNAPSHOT = {
    activeTheme: null,
    plugins: [
      {
        id: "dengliu.kubernetes",
        name: "Kubernetes",
        version: pluginVersion,
        description:
          "Manage Kubernetes clusters, Helm releases and Kustomize overlays: cluster explorer with live Pod logs, shell and port-forward, YAML completion, cross-resource navigation, gutter markers and manifest inspections.",
        authors: ["Deng Liu"],
        homepage: "https://github.com/dengliu/openidea-kubernetes",
        changeNotes: pluginVersion,
        icon: { light: pluginLogoSvg, dark: null },
        source: "linked",
        path: pluginRoot,
        enabled: true,
        status: "active",
        error: null,
        hasCode: true,
        publisher: "Deng Liu",
        capabilities: [
          { id: "project.read", risk: "low", description: "Read files", optional: false, granted: true },
          { id: "editor.read", risk: "low", description: "Read editor", optional: false, granted: true },
          { id: "editor.write", risk: "medium", description: "Write editor", optional: false, granted: true },
          { id: "events", risk: "low", description: "Observe events", optional: false, granted: true },
          { id: "storage", risk: "low", description: "Store state", optional: false, granted: true },
          { id: "clipboard", risk: "medium", description: "Clipboard", optional: false, granted: true },
          { id: "net:127.0.0.1", risk: "medium", description: "Loopback proxy", optional: false, granted: true },
          { id: "process:kubectl", risk: "high", description: "Run kubectl", optional: false, granted: true },
          { id: "process:helm", risk: "high", description: "Run helm", optional: true, granted: true },
          { id: "process:kustomize", risk: "high", description: "Run kustomize", optional: true, granted: true },
        ],
        netHosts: [],
        contributions: [
          { point: "views", label: "Kubernetes" },
          { point: "providers", label: "yaml, helm-template" },
          { point: "commands", label: "16 commands" },
        ],
        activation: [
          "on-startup-finished",
          "on-view:kubernetes",
          "on-language:yaml",
          "on-language:helm-template",
          "on-event:file-opened",
          "on-event:file-saved",
          "on-event:files-changed",
          "on-command:*",
        ],
        warnings: [],
      },
    ],
    ui: [
      {
        pluginId: "dengliu.kubernetes",
        point: "views",
        key: "kubernetes",
        decl: {
          id: "kubernetes",
          title: "Kubernetes",
          icon: "icons/kubernetes.svg",
          location: "right",
          secondary: true,
          web: "web/index.html",
          header: "none",
        },
      },
      {
        pluginId: "dengliu.kubernetes",
        point: "statusBar",
        key: "context",
        decl: {
          id: "context",
          text: `k8s: ${kubeContext}`,
          tooltip: "Kubernetes context and namespace",
          command: "switch-context",
          align: "right",
          priority: 120,
        },
      },
      ...COMMANDS.map((c) => ({ pluginId: "dengliu.kubernetes", point: "commands", key: c.id, decl: c })),
      ...MENUS.map((m, i) => ({ pluginId: "dengliu.kubernetes", point: "menus", key: `m${i}`, decl: m })),
      ...FILE_ICONS.map((f) => ({ pluginId: "dengliu.kubernetes", point: "fileIcons", key: f.id, decl: f })),
      {
        pluginId: "dengliu.kubernetes",
        point: "settings",
        key: "settings",
        decl: { title: "Kubernetes", schema: settingsSchema },
      },
    ],
  };

  class OpenIdeaServerClient {
    constructor(binPath, homeDir) {
      this.nextId = 1;
      this.pending = new Map();
      this.buffer = Buffer.alloc(0);
      this.onChannel = () => {};
      this.onEvent = () => {};
      this.onPluginEvent = () => {};
      this.child = spawn(binPath, ["--stdio"], {
        env: {
          ...process.env,
          PATH: `/opt/homebrew/bin:/usr/local/bin:${process.env.PATH || ""}`,
          OPENIDEA_SERVER_HOME: homeDir,
        },
        stdio: ["pipe", "pipe", "pipe"],
      });
      this.child.stdout.on("data", (chunk) => this.receive(chunk));
      this.child.stderr.on("data", (chunk) => {
        const s = chunk.toString();
        if (s.includes("ERROR") || s.includes("WARN")) process.stderr.write(`[server] ${s}`);
      });
    }
    async init() {
      return this.request("initialize", { version: 2 });
    }
    request(method, params) {
      const id = this.nextId++;
      return new Promise((resolve, reject) => {
        this.pending.set(id, { resolve, reject });
        const body = Buffer.from(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
        this.child.stdin.write(`Content-Length: ${body.length}\r\n\r\n`);
        this.child.stdin.write(body);
      });
    }
    async invoke(cmd, args) {
      try {
        return await this.request("invoke", { window: "main", cmd, args });
      } catch (e) {
        const msg = e?.message ?? String(e);
        try {
          throw JSON.parse(msg);
        } catch (parsed) {
          if (parsed instanceof SyntaxError) throw { kind: "protocol", message: msg, detail: null };
          throw parsed;
        }
      }
    }
    receive(chunk) {
      this.buffer = Buffer.concat([this.buffer, chunk]);
      for (;;) {
        const end = this.buffer.indexOf("\r\n\r\n");
        if (end < 0) return;
        const m = /content-length:\s*(\d+)/i.exec(this.buffer.subarray(0, end).toString());
        if (!m) throw new Error("Bad Content-Length header");
        const len = Number(m[1]);
        const start = end + 4;
        if (this.buffer.length < start + len) return;
        const msg = JSON.parse(this.buffer.subarray(start, start + len).toString("utf8"));
        this.buffer = this.buffer.subarray(start + len);
        if (msg.method && msg.id !== undefined) {
          const reply = Buffer.from(JSON.stringify({ jsonrpc: "2.0", id: msg.id, result: null }));
          this.child.stdin.write(`Content-Length: ${reply.length}\r\n\r\n`);
          this.child.stdin.write(reply);
        } else if (msg.method === "channel") {
          this.onChannel(msg.params.id, msg.params.data);
        } else if (msg.method === "event") {
          this.onEvent(msg.params.window ?? null, msg.params.event);
        } else if (msg.method === "plugin") {
          this.onPluginEvent(msg.params.window ?? null, msg.params.event);
        } else if (msg.id !== undefined) {
          const p = this.pending.get(msg.id);
          this.pending.delete(msg.id);
          if (msg.error) p?.reject(msg.error);
          else p?.resolve(msg.result);
        }
      }
    }
    close() {
      try {
        this.child.stdin.end();
        this.child.kill("SIGTERM");
      } catch {}
    }
  }

  const server = new OpenIdeaServerClient(join(openideaRoot, "target/release/openidea-server"), serverHome);
  await server.init();
  await server.invoke("project_trust_set", { path: projectRoot, trusted: true, folder: false });

  const probeSrv = http.createServer();
  await new Promise((r) => probeSrv.listen(0, "127.0.0.1", r));
  const vitePort = probeSrv.address().port;
  await new Promise((r) => probeSrv.close(r));

  const viteProc = spawn(
    join(openideaRoot, "node_modules/.bin/vite"),
    ["--port", String(vitePort), "--strictPort", "--host", "127.0.0.1"],
    {
      cwd: openideaRoot,
      env: { ...process.env, PATH: `/opt/homebrew/bin:/usr/local/bin:${process.env.PATH || ""}` },
      stdio: ["ignore", "pipe", "pipe"],
    },
  );

  await new Promise((resolveReady, rejectReady) => {
    const timeout = setTimeout(() => rejectReady(new Error("Vite timed out")), 30_000);
    const check = () => {
      http
        .get(`http://127.0.0.1:${vitePort}/`, (res) => {
          res.resume();
          clearTimeout(timeout);
          resolveReady();
        })
        .on("error", () => setTimeout(check, 250));
    };
    check();
  });

  const browser = await chromium.launch({ headless: true });
  const context = await browser.newContext({
    baseURL: `http://127.0.0.1:${vitePort}`,
    viewport: { width: 1920, height: 1080 },
    deviceScaleFactor: 1,
    colorScheme: "dark",
    userAgent:
      "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36",
  });

  let pluginSubscribeChannelId = null;
  let lastNativeTermId = null;
  let spawnedNativeTermCount = 0;
  let pageRef = null;
  let pendingNotes = [];
  let relayChain = Promise.resolve();

  function enqueueNote(kind, a, b) {
    pendingNotes.push([kind, a, b]);
    if (pendingNotes.length > 1) return;
    relayChain = relayChain
      .then(() => new Promise((r) => setImmediate(r)))
      .then(async () => {
        const batch = pendingNotes;
        pendingNotes = [];
        if (!pageRef) return;
        await pageRef
          .evaluate((notes) => {
            const shim = window.__openideaShim;
            if (!shim) return;
            for (const [k, x, y] of notes) {
              if (k === "channel") shim.channel(x, y);
              else shim.event(x, y);
            }
          }, batch)
          .catch(() => {});
      });
  }

  server.onChannel = (id, data) => enqueueNote("channel", id, data);
  server.onEvent = (win, ev) => enqueueNote("event", win, ev);
  server.onPluginEvent = (win, ev) => {
    if ((win === null || win === "main") && pluginSubscribeChannelId !== null) {
      enqueueNote("channel", pluginSubscribeChannelId, ev);
    }
  };

  await context.exposeFunction("__openideaRelay", async (cmd, args) => {
    try {
      if (cmd === "plugin_subscribe") {
        pluginSubscribeChannelId = args?.onEvent?.$channel ?? null;
        return { ok: null };
      }
      if (cmd === "plugin_list") return { ok: PLUGIN_SNAPSHOT };
      if (cmd === "plugin_assets") {
        const prefix = args?.prefix ?? "";
        return { ok: ASSET_ENTRIES.filter(([k]) => k.startsWith(prefix)) };
      }
      if (cmd === "plugin_web_page") return { ok: { path: `dengliu.kubernetes/${pluginVersion}/web/index.html` } };
      if (cmd === "plugin_market") return { ok: [] };
      if (cmd === "plugin_market_url") return { ok: null };
      if (cmd === "plugin_catalog") return { ok: [] };
      if (cmd === "plugin_installed_intellij_themes") return { ok: [] };
      if (cmd === "plugin_secrets") return { ok: [] };
      if (cmd === "plugin_activation_event") return { ok: null };
      if (cmd === "plugin_web_pty_spawn") {
        const c = args.command || {};
        const prog = c.program === "kubectl" ? "/usr/local/bin/kubectl" : c.program;
        const val = await server.invoke("term_spawn", {
          options: {
            cwd: projectRoot,
            shell: prog,
            args: c.args || [],
            env: c.env || {},
            cols: c.cols || 180,
            rows: c.rows || 40,
            shellIntegration: false,
          },
          onOutput: args.onOutput,
        });
        return { ok: val };
      }
      const val = await server.invoke(cmd, args);
      if (cmd === "term_spawn" && val && typeof val === "object" && "id" in val) {
        lastNativeTermId = val.id;
        spawnedNativeTermCount++;
      }
      return { ok: val ?? null };
    } catch (err) {
      return { err };
    }
  });

  const { installTauriShim } = await import(join(openideaRoot, "smoke/tauriShim.ts"));
  const { installInPage } = await import(join(openideaRoot, "perf/lib/inpage.ts"));

  await context.addInitScript(() => {
    Object.defineProperty(Navigator.prototype, "platform", { get: () => "MacIntel" });
  });
  await context.addInitScript(installTauriShim, { directories: { 12: tmpdir(), 21: homedir() }, sep });
  await context.addInitScript(() => {
    window.__openideaForward = async (cmd, args) => {
      const r = await window.__openideaRelay(cmd, args);
      if ("err" in r) throw r.err;
      return r.ok;
    };
  });
  await context.addInitScript(installInPage);

  const page = await context.newPage();
  pageRef = page;

  try {
    await page.goto("/");
    await page.waitForFunction(() => "__openideaPerf" in window && "__openideaShim" in window);

    await page.evaluate(async (port) => {
      const backendMod = await import("/src/features/plugins/backend.ts");
      const storeMod = await import("/src/features/plugins/store.ts");
      backendMod.setPluginsBackend({
        ...backendMod.tauriPluginsBackend,
        webPageUrl: async () => `http://127.0.0.1:${port}/web/index.html`,
      });
      await storeMod.refreshPlugins();
    }, webStaticPort);

    await page.evaluate((root) => window.__openideaPerf.openProject(root), projectRoot);
    await page.waitForSelector('[role="tree"][aria-label="Project"] [role="treeitem"]', { timeout: 30_000 });
    await page.locator("#splash").waitFor({ state: "detached", timeout: 15_000 }).catch(() => {});

    await page.evaluate(async () => {
      document.documentElement.dataset.theme = "dark";
      const storeMod = await import("/src/features/plugins/store.ts");
      await storeMod.refreshPlugins();
    });
    await page.waitForTimeout(700);

    // Inject cinema showcase UI overlay
    await page.evaluate(() => {
      const style = document.createElement("style");
      style.textContent = `
        #demo-overlay-root {
          position: fixed;
          inset: 0;
          pointer-events: none;
          z-index: 2147483640;
          font-family: -apple-system, BlinkMacSystemFont, "Inter", "SF Pro Display", sans-serif;
        }
        #demo-cursor {
          position: fixed;
          left: 960px;
          top: 540px;
          width: 26px;
          height: 26px;
          transform: translate(-3px, -2px);
          filter: drop-shadow(0 3px 8px rgba(0, 0, 0, 0.65));
          z-index: 2147483647;
        }
        .demo-ripple {
          position: fixed;
          width: 44px;
          height: 44px;
          margin-left: -22px;
          margin-top: -22px;
          border-radius: 50%;
          border: 2.5px solid #58a6ff;
          background: rgba(50, 108, 229, 0.28);
          box-shadow: 0 0 18px rgba(88, 166, 255, 0.65);
          transform: scale(0.35);
          opacity: 0.95;
          pointer-events: none;
          z-index: 2147483646;
          animation: demo-ripple-anim 480ms cubic-bezier(0.16, 1, 0.3, 1) forwards;
        }
        @keyframes demo-ripple-anim {
          to { transform: scale(1.55); opacity: 0; }
        }
        #demo-callout {
          position: fixed;
          left: 28px;
          bottom: 42px;
          max-width: 560px;
          padding: 13px 19px;
          border-radius: 14px;
          background: linear-gradient(145deg, rgba(15, 21, 32, 0.94), rgba(20, 29, 47, 0.92));
          border: 1px solid rgba(88, 166, 255, 0.42);
          box-shadow: 0 16px 40px rgba(0, 0, 0, 0.62), 0 0 0 1px rgba(50, 108, 229, 0.2) inset;
          color: #f0f6fc;
          backdrop-filter: blur(12px);
          transition: opacity 220ms ease, transform 220ms ease;
        }
        #demo-callout.hidden {
          opacity: 0;
          transform: translateY(10px);
        }
        #demo-callout-badge {
          display: inline-flex;
          align-items: center;
          gap: 7px;
          font-size: 11px;
          font-weight: 700;
          letter-spacing: 0.06em;
          text-transform: uppercase;
          color: #79c0ff;
          background: rgba(50, 108, 229, 0.24);
          border: 1px solid rgba(88, 166, 255, 0.35);
          padding: 3px 10px;
          border-radius: 999px;
          margin-bottom: 6px;
        }
        #demo-callout-title {
          font-size: 18px;
          font-weight: 700;
          line-height: 1.25;
          color: #ffffff;
          margin-bottom: 4px;
        }
        #demo-callout-sub {
          font-size: 13px;
          line-height: 1.45;
          color: #b6c2cf;
        }
        #demo-hero {
          position: fixed;
          inset: 0;
          display: flex;
          align-items: center;
          justify-content: center;
          background: radial-gradient(circle at 50% 38%, rgba(26, 48, 94, 0.88), rgba(9, 13, 22, 0.94));
          backdrop-filter: blur(8px);
          opacity: 0;
          transition: opacity 280ms ease;
        }
        #demo-hero.visible { opacity: 1; }
        #demo-hero.visible ~ #demo-cursor { opacity: 0; }
        .demo-hero-card {
          text-align: center;
          max-width: 920px;
          padding: 44px 58px;
          border-radius: 24px;
          background: linear-gradient(160deg, rgba(20, 28, 44, 0.96), rgba(13, 18, 28, 0.96));
          border: 1px solid rgba(88, 166, 255, 0.45);
          box-shadow: 0 28px 80px rgba(0, 0, 0, 0.75);
        }
        .demo-hero-logo { width: 74px; height: 74px; margin: 0 auto 18px; }
        .demo-hero-title { font-size: 42px; font-weight: 800; letter-spacing: -0.02em; color: #ffffff; margin-bottom: 10px; }
        .demo-hero-sub { font-size: 19px; line-height: 1.5; color: #9ecbff; margin-bottom: 22px; }
        .demo-hero-pills { display: flex; flex-wrap: wrap; justify-content: center; gap: 10px; }
        .demo-hero-pill {
          padding: 6px 14px;
          border-radius: 999px;
          font-size: 13.5px;
          font-weight: 600;
          color: #e6edf3;
          background: rgba(50, 108, 229, 0.26);
          border: 1px solid rgba(88, 166, 255, 0.4);
        }
      `;
      document.head.appendChild(style);

      const root = document.createElement("div");
      root.id = "demo-overlay-root";
      root.innerHTML = `
        <div id="demo-hero" class="visible">
          <div class="demo-hero-card">
            <div class="demo-hero-logo">
              <svg viewBox="0 0 24 24" width="74" height="74" fill="none">
                <polygon points="12,1.65 20.44,5.72 22.53,14.85 16.69,22.18 7.31,22.18 1.47,14.85 3.56,5.72" fill="#326CE5" stroke="#58a6ff" stroke-width="1.2" stroke-linejoin="round"/>
                <circle cx="12" cy="12" r="3.2" stroke="#fff" stroke-width="1.6"/>
                <path d="M12 4.8v3.4M12 15.8v3.4M5.4 8.6l3 1.7M15.6 14.3l3 1.7M18.6 8.6l-3 1.7M8.4 14.3l-3 1.7" stroke="#fff" stroke-width="1.5" stroke-linecap="round"/>
              </svg>
            </div>
            <div class="demo-hero-title">Kubernetes for OpenIDEA</div>
            <div class="demo-hero-sub">Freelens-Style Cluster Explorer + Ephemeral Debug Shells, Live Log Streaming, Spec Diff/Apply &amp; YAML/Helm/Kustomize Intelligence</div>
            <div class="demo-hero-pills">
              <span class="demo-hero-pill">32+ Resource Kinds &amp; CRDs</span>
              <span class="demo-hero-pill">Pod &amp; Ephemeral Debug Shells</span>
              <span class="demo-hero-pill">Live Log Streaming Editor</span>
              <span class="demo-hero-pill">Native Spec Reload / Diff / Apply</span>
              <span class="demo-hero-pill">Helm Release Inspector</span>
              <span class="demo-hero-pill">YAML, Helm &amp; Kustomize Intelligence</span>
            </div>
          </div>
        </div>
        <div id="demo-callout" class="hidden">
          <div id="demo-callout-badge"></div>
          <div id="demo-callout-title"></div>
          <div id="demo-callout-sub"></div>
        </div>
        <div id="demo-cursor">
          <svg width="26" height="26" viewBox="0 0 26 26" fill="none">
            <path d="M3 2.2L21.5 11.2L13.2 13.8L10.2 22.2L3 2.2Z" fill="#ffffff" stroke="#0d1117" stroke-width="1.8" stroke-linejoin="round"/>
          </svg>
        </div>
      `;
      document.body.appendChild(root);

      window.__demoUi = {
        setCursor(x, y) {
          const el = document.getElementById("demo-cursor");
          if (el) {
            el.style.left = `${Math.round(x)}px`;
            el.style.top = `${Math.round(y)}px`;
          }
        },
        ripple(x, y) {
          const r = document.createElement("div");
          r.className = "demo-ripple";
          r.style.left = `${Math.round(x)}px`;
          r.style.top = `${Math.round(y)}px`;
          document.getElementById("demo-overlay-root")?.appendChild(r);
          setTimeout(() => r.remove(), 520);
        },
        setCallout(badge, title, sub, pos = "bottom-left") {
          const c = document.getElementById("demo-callout");
          if (!c) return;
          if (!title) {
            c.classList.add("hidden");
            return;
          }
          document.getElementById("demo-callout-badge").textContent = badge;
          document.getElementById("demo-callout-title").textContent = title;
          document.getElementById("demo-callout-sub").textContent = sub;
          c.style.top = "auto";
          c.style.bottom = "auto";
          c.style.left = "auto";
          c.style.right = "auto";
          if (pos === "top-left") {
            c.style.top = "86px";
            c.style.left = "54px";
          } else if (pos === "bottom-right") {
            c.style.bottom = "42px";
            c.style.right = "28px";
          } else {
            c.style.bottom = "42px";
            c.style.left = "28px";
          }
          c.classList.remove("hidden");
        },
        setHero(visible, title, sub, pills) {
          const h = document.getElementById("demo-hero");
          if (!h) return;
          if (title) h.querySelector(".demo-hero-title").textContent = title;
          if (sub) h.querySelector(".demo-hero-sub").textContent = sub;
          if (pills) {
            h.querySelector(".demo-hero-pills").innerHTML = pills
              .map((p) => `<span class="demo-hero-pill">${p}</span>`)
              .join("");
          }
          h.classList.toggle("visible", !!visible);
        },
      };
    });

    const openWsFile = async (rel) => {
      const abs = join(projectRoot, rel);
      const base = rel.split("/").pop();
      await page.evaluate((p) => window.__openideaPerf.openFile(p), abs);
      await page.locator('[role="tab"][aria-selected="true"]', { hasText: base }).waitFor({ timeout: 15_000 });
      await page.locator(".cm-content .cm-line").first().waitFor({ timeout: 15_000 });
    };

    // Warm cross-resource YAML/Helm/Kustomize index & publish dynamic kube-app icons once
    await openWsFile("plain-manifest/secret.yaml");
    await openWsFile("plain-manifest/service.yaml");
    await openWsFile("plain-manifest/hpa.yaml");
    await openWsFile("simple-app-chart/values.yaml");
    await openWsFile("simple-app-kustomize/overlays/prod/kustomization.yaml");
    await openWsFile("plain-manifest/deployment.yaml");
    await page.waitForTimeout(800);

    const dontShowBtn = page.getByText("Don't Show Again", { exact: true }).first();
    if (await dontShowBtn.isVisible().catch(() => false)) await dontShowBtn.click().catch(() => {});
    const closeToastBtns = page.locator('[aria-label="Close notification"], [title="Dismiss"], [title="Close"]');
    const toastCnt = await closeToastBtns.count().catch(() => 0);
    for (let i = 0; i < toastCnt; i++) await closeToastBtns.nth(i).click().catch(() => {});

    const k8sFrame = page.frameLocator('iframe[data-plugin-webview="dengliu.kubernetes"]');

    async function setAnchorSize(anchor, px) {
      await page.evaluate(
        async ([a, sizePx]) => {
          const twStore = await import("/src/workbench/toolwindows/store.ts");
          twStore.setToolWindowSize(a, sizePx);
        },
        [anchor, px],
      );
      await page.waitForTimeout(160);
    }

    // Idempotently sets up exact IDE + Cluster Explorer state BEFORE frame 1 of a scene!
    async function applyPreset(preset = {}, scene) {
      await page.keyboard.press("Escape").catch(() => {});
      if (preset.terminalOpen === false) {
        await page.evaluate(async () => {
          const contrib = await import("/src/workbench/contributions.ts");
          contrib.toolWindowService().hide("terminal");
        });
      }
      if (preset.closeAllEditorTabs || preset.resetManifests) {
        await page.evaluate(() => window.__openideaPerf.runAction("editor.closeAllTabs"));
        await page.waitForTimeout(100);
      }
      if (preset.resetManifests) {
        writeCleanManifests();
      }
      if (typeof preset.projectSidebar === "boolean") {
        await page.evaluate(async (showProj) => {
          const contrib = await import("/src/workbench/contributions.ts");
          if (showProj) contrib.toolWindowService().show("project", false);
          else contrib.toolWindowService().hide("project");
        }, preset.projectSidebar);
        await page.waitForTimeout(180);
        if (preset.projectSidebar) {
          await setAnchorSize("left", preset.leftWidth ?? 320);
          for (const folderName of [
            "plain-manifest",
            "simple-app-chart",
            "templates",
            "simple-app-kustomize",
            "overlays",
            "dev",
            "prod",
          ]) {
            const item = page.getByRole("treeitem", { name: new RegExp(`^${folderName}(\\b|$)`) }).first();
            if (await item.isVisible().catch(() => false)) {
              const expanded = await item.getAttribute("aria-expanded");
              if (expanded === "false") {
                await item.click().catch(() => {});
                await page.keyboard.press("ArrowRight").catch(() => {});
                await page.waitForTimeout(80);
              }
            }
          }
        }
      }
      if (preset.openFile) {
        await openWsFile(preset.openFile);
        await page.waitForTimeout(400);
      }
      if (typeof preset.bottomHeight === "number") {
        await setAnchorSize("bottom", preset.bottomHeight);
      }
      if (typeof preset.explorerOpen === "boolean") {
        const isVis = await page.locator('iframe[data-plugin-webview="dengliu.kubernetes"]').isVisible().catch(() => false);
        if (preset.explorerOpen && !isVis) {
          await page.evaluate(async () => {
            const contrib = await import("/src/workbench/contributions.ts");
            contrib.toolWindowService().show("plugin.dengliu.kubernetes.kubernetes", false);
          });
          await page.locator('iframe[data-plugin-webview="dengliu.kubernetes"]').waitFor({ state: "visible", timeout: 15_000 });
          await k8sFrame.locator(".fl-nav-tree").waitFor({ state: "visible", timeout: 20_000 });
          await page.waitForTimeout(1800);
        } else if (!preset.explorerOpen && isVis) {
          await page.evaluate(async () => {
            const contrib = await import("/src/workbench/contributions.ts");
            contrib.toolWindowService().hide("plugin.dengliu.kubernetes.kubernetes");
          });
          await page.waitForTimeout(180);
        }
      }
      if (preset.explorerOpen && typeof preset.rightWidth === "number") {
        await setAnchorSize("right", preset.rightWidth);
      }
      if (preset.explorerOpen && preset.drawerClosed) {
        const closeBtn = k8sFrame.locator('button[title="Close"]').first();
        if (await closeBtn.isVisible().catch(() => false)) {
          await closeBtn.dispatchEvent("click");
          await page.waitForTimeout(200);
        }
      }
      if (preset.explorerOpen && preset.explorerKind) {
        const kindMap = {
          Overview: { group: null, label: /^Overview$/ },
          Node: { group: null, label: /^Nodes$/ },
          Pod: { group: "Workloads", label: /^Pods$/ },
          Deployment: { group: "Workloads", label: /^Deployments$/ },
          HelmRelease: { group: "Helm", label: /^Releases$/ },
          CustomResourceDefinition: { group: "Custom Resources", label: /^Definitions$/ },
        };
        const target = kindMap[preset.explorerKind];
        if (target) {
          if (!target.group) {
            const hdr = k8sFrame.locator(".fl-tree-group-header", { hasText: target.label }).first();
            await hdr.dispatchEvent("click");
          } else {
            const child = k8sFrame.locator(".fl-tree-child", { hasText: target.label }).first();
            if (!(await child.isVisible().catch(() => false))) {
              const grp = k8sFrame.locator(".fl-tree-group-header", { hasText: target.group }).first();
              await grp.dispatchEvent("click");
              await page.waitForTimeout(180);
            }
            await child.dispatchEvent("click");
          }
          await page.waitForTimeout(800);
        }
      }
      if (preset.explorerOpen && preset.namespace) {
        const nsPickerBtn = k8sFrame.locator(".fl-ns-picker-btn").first();
        if (await nsPickerBtn.isVisible().catch(() => false)) {
          await nsPickerBtn.dispatchEvent("click");
          await page.waitForTimeout(200);
          if (preset.namespace === "all") {
            const allOpt = k8sFrame.locator(".fl-ns-pinned .fl-ns-opt", { hasText: "All Namespaces" }).first();
            if (!(await allOpt.locator(".fl-ns-check.checked").count().catch(() => 0))) {
              await allOpt.dispatchEvent("click");
              await page.waitForTimeout(300);
            }
          } else if (typeof preset.namespace === "string") {
            const onlyBtn = k8sFrame
              .locator(".fl-ns-list .fl-ns-opt")
              .filter({ has: k8sFrame.locator(".fl-ns-opt-label", { hasText: new RegExp(`^${preset.namespace}$`) }) })
              .locator(".fl-ns-only-btn")
              .first();
            if (await onlyBtn.isVisible().catch(() => false)) {
              await onlyBtn.dispatchEvent("click");
              await page.waitForTimeout(300);
            }
          }
          if (await k8sFrame.locator(".fl-ns-popover").isVisible().catch(() => false)) {
            await nsPickerBtn.dispatchEvent("click");
            await page.waitForTimeout(180);
          }
          await page.waitForTimeout(600);
        }
      }

      // Configure initial Hero / Callout state for this scene
      if (preset.hero) {
        await page.evaluate(() => {
          window.__demoUi.setCallout("", "", "");
          window.__demoUi.setHero(
            true,
            "Kubernetes for OpenIDEA",
            "Freelens-Style Cluster Explorer + Ephemeral Debug Shells, Live Log Streaming, Spec Diff/Apply & YAML/Helm/Kustomize Intelligence",
            [
              "32+ Resource Kinds & CRDs",
              "Pod & Ephemeral Debug Shells",
              "Live Log Streaming Editor",
              "Native Spec Reload / Diff / Apply",
              "Helm Release Inspector",
              "YAML, Helm & Kustomize Intelligence",
            ],
          );
        });
      } else {
        await page.evaluate(
          ([b, t, s, p]) => {
            window.__demoUi.setHero(false);
            window.__demoUi.setCallout(b, t, s, p);
          },
          [scene.badge || "", scene.title || "", scene.subtitle || "", scene.calloutPos || "bottom-left"],
        );
      }
      await page.waitForTimeout(150);
    }

    // Record each dirty/requested scene into its isolated `.cache/scenes/<id>/` folder!
    let cursorPos = { x: 960, y: 540 };

    for (const item of scenesToRecord) {
      const { scene, uiHash } = item;
      console.log(`==> Capturing scene [${scene.id}] (${scene.title || "Hero/Outro"})...`);

      const sceneDir = join(sceneCacheRoot, scene.id);
      const framesDir = join(sceneDir, "frames");
      rmSync(framesDir, { recursive: true, force: true });
      mkdirSync(framesDir, { recursive: true });

      const rawTimeline = [];
      let seq = 0;

      await applyPreset(scene.preset, scene);

      const snap = async (silentMs = 66, narratedBaseMs = silentMs) => {
        seq++;
        const relFile = `frame_${String(seq).padStart(5, "0")}.png`;
        await page.screenshot({ path: join(framesDir, relFile), type: "png" });
        rawTimeline.push({
          file: relFile,
          silentMs: Math.max(33, Math.round(silentMs)),
          narratedBaseMs: Math.max(33, Math.round(narratedBaseMs)),
        });
      };

      const hold = async (totalMs) => {
        if (totalMs <= 0) return;
        const beats = Math.max(1, Math.round(totalMs / 320));
        const perBeat = Math.max(66, Math.round(totalMs / beats));
        for (let i = 0; i < beats; i++) {
          await page.waitForTimeout(Math.min(180, perBeat));
          await snap(perBeat, perBeat);
        }
      };

      const moveCursorTo = async (targetX, targetY, durationMs = 360) => {
        const steps = Math.max(5, Math.round(durationMs / 48));
        const startX = cursorPos.x;
        const startY = cursorPos.y;
        const stepDur = Math.max(33, Math.round(durationMs / steps));
        for (let i = 1; i <= steps; i++) {
          const t = i / steps;
          const ease = t < 0.5 ? 2 * t * t : 1 - Math.pow(-2 * t + 2, 2) / 2;
          const nx = startX + (targetX - startX) * ease;
          const ny = startY + (targetY - startY) * ease;
          await page.evaluate(([x, y]) => window.__demoUi.setCursor(x, y), [nx, ny]);
          await page.mouse.move(nx, ny);
          cursorPos = { x: nx, y: ny };
          await snap(stepDur, stepDur);
        }
      };

      const clickBox = async (box, opts = {}) => {
        const tx = box.x + (opts.offsetX ?? box.width / 2);
        const ty = box.y + (opts.offsetY ?? box.height / 2);
        await moveCursorTo(tx, ty, opts.moveMs ?? 340);
        await page.evaluate(([x, y]) => window.__demoUi.ripple(x, y), [tx, ty]);
        await page.mouse.click(tx, ty, { button: opts.button || "left" });
        await page.waitForTimeout(80);
        await snap(100, 100);
      };

      const clickLocator = async (locator, opts = {}) => {
        await locator.waitFor({ state: "visible", timeout: 15_000 });
        const box = await locator.boundingBox();
        if (!box) throw new Error("Locator has no boundingBox");
        await clickBox(box, opts);
      };

      const clickInFrame = async (locator, opts = {}) => {
        await locator.waitFor({ state: "visible", timeout: 20_000 });
        const box = await locator.boundingBox();
        if (!box) throw new Error("Frame locator has no boundingBox");
        await clickBox(box, opts);
      };

      const closeExplorerDrawer = async () => {
        const btn = k8sFrame.locator('button[title="Close"]').first();
        if (await btn.isVisible().catch(() => false)) {
          await btn.dispatchEvent("click");
          await page.waitForTimeout(220);
          await snap(100, 100);
        }
      };

      const clickDrawerHeaderAction = async (titleAttr) => {
        const btn = k8sFrame.locator(`button[title="${titleAttr}"]`).first();
        await btn.waitFor({ state: "visible", timeout: 15_000 });
        const box = await btn.boundingBox();
        if (box) {
          await moveCursorTo(box.x + box.width / 2, box.y + box.height / 2, 280);
          await page.evaluate(
            ([x, y]) => window.__demoUi.ripple(x, y),
            [box.x + box.width / 2, box.y + box.height / 2],
          );
        }
        await btn.dispatchEvent("click");
        await page.waitForTimeout(180);
        await snap(100, 100);
      };

      const typeInActiveTerminal = async (cmdText) => {
        if (lastNativeTermId === null) return;
        for (const ch of cmdText) {
          await server.invoke("term_write", { id: lastNativeTermId, data: ch });
          await page.waitForTimeout(30);
          if (Math.random() < 0.35) await snap(55, 55);
        }
        await server.invoke("term_write", { id: lastNativeTermId, data: "\r" });
        await page.waitForTimeout(700);
        await hold(1350);
      };

      const ctx = {
        page,
        k8sFrame,
        pluginVersion,
        kubeContext,
        multiPodName,
        snap,
        hold,
        moveCursorTo,
        clickLocator,
        clickInFrame,
        closeExplorerDrawer,
        clickDrawerHeaderAction,
        openWsFile,
        setAnchorSize,
        setCallout: (b, t, s, p = "bottom-left") =>
          page.evaluate(([x1, x2, x3, x4]) => window.__demoUi.setCallout(x1, x2, x3, x4), [b, t, s, p]),
        showSceneCallout: () =>
          page.evaluate(
            ([b, t, s, p]) => window.__demoUi.setCallout(b, t, s, p),
            [scene.badge || "", scene.title || "", scene.subtitle || "", scene.calloutPos || "bottom-left"],
          ),
        setHero: (v, t, s, p) => page.evaluate(([x1, x2, x3, x4]) => window.__demoUi.setHero(x1, x2, x3, x4), [v, t, s, p]),
        getSpawnedTermCount: () => spawnedNativeTermCount,
        typeInActiveTerminal,
        scaleDeployment,
        cleanDebugWatcherPods,
      };

      try {
        await scene.record(ctx);
      } finally {
        if (typeof scene.cleanup === "function") {
          await scene.cleanup(ctx);
        }
      }

      writeFileSync(join(sceneDir, "raw_timeline.json"), JSON.stringify({ uiHash, frames: rawTimeline }, null, 2));
      console.log(`    Captured ${rawTimeline.length} frames for [${scene.id}]`);
    }
  } finally {
    scaleDeployment("podinfo", "default", 1);
    cleanDebugWatcherPods();
    await browser.close().catch(() => {});
    viteProc.kill("SIGTERM");
    webStaticServer.close();
    server.close();
    rmSync(workDir, { recursive: true, force: true });
  }
}
