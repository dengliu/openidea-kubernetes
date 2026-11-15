#!/usr/bin/env node
/**
 * Modular Scene-Clip Demo Pipeline CLI (`scripts/demo/cli.mjs`).
 *
 * Commands:
 *   cli.mjs list
 *       Lists all scene modules in `scenes/`, their playlist status, and cache status (`cached` vs `dirty`).
 *   cli.mjs build [--mode both|silent|narrated] [--scene <id[,id2]>] [--force] [--voice <voice>] [--out-dir <dir>]
 *       Incrementally records ONLY uncached/modified scenes (or `--scene` targets), encodes per-scene `.mp4`
 *       clips, and losslessly stitches all enabled `playlist.mjs` clips into the final 1080p MP4s in < 1s!
 *   cli.mjs preview <id> [--mode both|silent|narrated] [--force]
 *       Records/updates a single scene in isolation and writes `preview-<id>.mp4` for fast iteration (~8s).
 *   cli.mjs new <id> [--title "..."] [--badge "..."]
 *       Scaffolds `scripts/demo/scenes/<id>.mjs` and appends `<id>` before `99-outro` in `playlist.mjs`.
 *   cli.mjs disable <id> | enable <id>
 *       Removes or re-adds `<id>` in `playlist.mjs` without deleting its scene source or cached clips.
 */
import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { captureScenes } from "./harness.mjs";

const SCRIPT_DIR = import.meta.dirname;
const PLUGIN_ROOT = process.env.PLUGIN_ROOT || resolve(SCRIPT_DIR, "../..");
const OPENIDEA_ROOT = process.env.OPENIDEA_ROOT || resolve(PLUGIN_ROOT, "../openidea");
const KUBE_APP_ROOT = process.env.KUBE_APP_ROOT || join(SCRIPT_DIR, "fixtures/kube-app");
const SCENES_DIR = join(SCRIPT_DIR, "scenes");
const PLAYLIST_FILE = join(SCRIPT_DIR, "playlist.mjs");
const CACHE_DIR = join(SCRIPT_DIR, ".cache");
const BIN_DIR = join(CACHE_DIR, "bin");
const TTS_CACHE_DIR = join(CACHE_DIR, "tts");
const SCENE_CACHE_DIR = join(CACHE_DIR, "scenes");
const VENV_DIR = join(CACHE_DIR, "venv");
const WORK_DIR = join(SCRIPT_DIR, ".work");

function sha256Short(text) {
  return createHash("sha256").update(text).digest("hex").slice(0, 20);
}

function parseFlags(argv) {
  const positional = [];
  const flags = {};
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a.startsWith("--")) {
      const k = a.slice(2);
      const next = argv[i + 1];
      if (!next || next.startsWith("--")) {
        flags[k] = true;
      } else {
        flags[k] = next;
        i++;
      }
    } else {
      positional.push(a);
    }
  }
  return { positional, flags };
}

function hashDirectoryContents(rootDir) {
  if (!existsSync(rootDir)) return "";
  const h = createHash("sha256");
  const walk = (dir, relPrefix = "") => {
    const entries = readdirSync(dir, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name));
    for (const ent of entries) {
      if (ent.name === ".git" || ent.name === ".idea" || ent.name === ".DS_Store") continue;
      const abs = join(dir, ent.name);
      const rel = relPrefix ? `${relPrefix}/${ent.name}` : ent.name;
      if (ent.isDirectory()) {
        walk(abs, rel);
      } else if (ent.isFile()) {
        h.update(`file:${rel}\0`);
        h.update(readFileSync(abs));
        h.update("\0");
      }
    }
  };
  walk(rootDir);
  return h.digest("hex").slice(0, 16);
}

async function loadAllScenes() {
  const kubeAppDigest = hashDirectoryContents(KUBE_APP_ROOT);
  const files = readdirSync(SCENES_DIR)
    .filter((f) => f.endsWith(".mjs"))
    .sort();
  const map = new Map();
  for (const f of files) {
    const full = join(SCENES_DIR, f);
    const mod = await import(`${pathToFileURL(full).href}?t=${statSync(full).mtimeMs}`);
    const s = mod.default;
    if (!s?.id) throw new Error(`Scene file ${f} has no default export with 'id'`);
    const usesWorkspace = Boolean(s.preset?.projectSidebar || s.preset?.openFile || s.preset?.resetManifests);
    const uiHash = sha256Short(
      JSON.stringify({
        id: s.id,
        badge: s.badge || "",
        title: s.title || "",
        subtitle: s.subtitle || "",
        calloutPos: s.calloutPos || "bottom-left",
        preset: s.preset || {},
        recordSrc: String(s.record),
        kubeAppDigest: usesWorkspace ? kubeAppDigest : "",
      }),
    );
    map.set(s.id, { file: f, path: full, scene: s, uiHash });
  }
  return map;
}

async function loadPlaylist() {
  const mod = await import(`${pathToFileURL(PLAYLIST_FILE).href}?t=${statSync(PLAYLIST_FILE).mtimeMs}`);
  return [...(mod.PLAYLIST || [])];
}

function savePlaylist(ids) {
  const lines = [
    "/**",
    " * Ordered playlist of enabled demo scenes.",
    " *",
    " * - Remove or comment out any scene ID to exclude it from the final stitched MP4s (< 1s re-stitch!).",
    " * - Reorder IDs freely — every scene has its own declarative `preset` and cached clips.",
    " * - Add a new scene with `make demo_new ID=85-port-forward TITLE=\"One-Click Port Forwarding\"`.",
    " */",
    "export const PLAYLIST = [",
    ...ids.map((id) => `  ${JSON.stringify(id)},`),
    "];",
    "",
  ];
  writeFileSync(PLAYLIST_FILE, lines.join("\n"));
}

function ensureBinaries() {
  mkdirSync(BIN_DIR, { recursive: true });
  const swiftTools = ["encode_mp4", "mux_av", "stitch_clips"];
  for (const name of swiftTools) {
    const src = join(SCRIPT_DIR, `${name}.swift`);
    const out = join(BIN_DIR, name);
    if (!existsSync(out) || statSync(src).mtimeMs > statSync(out).mtimeMs) {
      console.log(`==> Compiling ${name}...`);
      execFileSync("swiftc", ["-O", src, "-o", out], { stdio: "inherit" });
    }
  }
  const serverBin = join(OPENIDEA_ROOT, "target/release/openidea-server");
  const serverWatchFiles = [
    join(OPENIDEA_ROOT, "crates/ide-plugin/src/runtime.rs"),
    join(OPENIDEA_ROOT, "crates/ide-host/src/bin/server.rs"),
  ];
  const serverStale =
    !existsSync(serverBin) ||
    serverWatchFiles.some((f) => existsSync(f) && statSync(f).mtimeMs > statSync(serverBin).mtimeMs);
  if (serverStale) {
    console.log("==> Building up-to-date release openidea-server...");
    execFileSync(
      "cargo",
      ["build", "--manifest-path", join(OPENIDEA_ROOT, "Cargo.toml"), "--release", "-p", "ide-host", "--bin", "openidea-server"],
      { stdio: "inherit" },
    );
  }
}

function ensureTtsVenv() {
  const py = join(VENV_DIR, "bin/python3");
  if (!existsSync(join(VENV_DIR, "bin/edge-tts"))) {
    console.log("==> Creating cached edge-tts virtualenv...");
    execFileSync("python3", ["-m", "venv", VENV_DIR], { stdio: "inherit" });
    execFileSync(join(VENV_DIR, "bin/pip"), ["install", "-q", "edge-tts"], { stdio: "inherit" });
  }
  return py;
}

function computeSimulatedEncodedMs(entries) {
  const stepMs = 33;
  let currentMs = 0;
  for (const e of entries) {
    const dur = Math.max(Math.round(e.durationMs), stepMs);
    const endMs = currentMs + dur;
    let t = currentMs;
    while (t < endMs) t += stepMs;
    currentMs = t;
  }
  return currentMs;
}

async function cmdList() {
  const scenesMap = await loadAllScenes();
  const playlist = await loadPlaylist();
  const enabledSet = new Set(playlist);

  console.log("ID                       PLAYLIST   FRAMES CACHE   CLIPS CACHE    TITLE");
  console.log("--------------------------------------------------------------------------------------------------------");
  for (const [id, info] of scenesMap.entries()) {
    const sDir = join(SCENE_CACHE_DIR, id);
    const rawPath = join(sDir, "raw_timeline.json");
    const metaPath = join(sDir, "clip_meta.json");
    let frameState = "missing";
    if (existsSync(rawPath)) {
      const raw = JSON.parse(readFileSync(rawPath, "utf8"));
      frameState = raw.uiHash === info.uiHash ? "up-to-date" : "modified";
    }
    let clipState = "missing";
    if (existsSync(metaPath) && existsSync(join(sDir, "clip-silent.mp4"))) {
      const meta = JSON.parse(readFileSync(metaPath, "utf8"));
      clipState = meta.uiHash === info.uiHash ? "ready" : "stale";
    }
    const orderIdx = playlist.indexOf(id);
    const plTag = enabledSet.has(id) ? `#${String(orderIdx + 1).padStart(2, "0")} enabled` : "disabled";
    console.log(
      `${id.padEnd(24)} ${plTag.padEnd(10)} ${frameState.padEnd(14)} ${clipState.padEnd(14)} ${info.scene.title || "(Hero/Outro)"}`,
    );
  }
}

async function cmdNew(id, flags) {
  if (!id || !/^[a-z0-9][a-z0-9_-]*$/i.test(id)) {
    throw new Error("Usage: cli.mjs new <id> [--title '...'] [--badge '...'] (e.g. 85-port-forward)");
  }
  const targetPath = join(SCENES_DIR, `${id}.mjs`);
  if (existsSync(targetPath)) throw new Error(`Scene ${targetPath} already exists`);

  const title = flags.title || "Showcase New Feature";
  const badge = (flags.badge || id.replace(/^\d+-/, "").replace(/-/g, " ")).toUpperCase();

  writeFileSync(
    targetPath,
    `export default {
  id: ${JSON.stringify(id)},
  badge: ${JSON.stringify(badge)},
  title: ${JSON.stringify(title)},
  subtitle: "Describe what makes this feature compelling for Kubernetes developers.",
  calloutPos: "bottom-left",
  voiceover: "Explain this feature in one natural spoken sentence for Open IDEA users.",
  preset: {
    hero: false,
    projectSidebar: false,
    closeAllEditorTabs: true,
    explorerOpen: true,
    rightWidth: 1220,
    terminalOpen: false,
    drawerClosed: true,
    namespace: "default",
    explorerKind: "Pod",
  },
  async record(ctx) {
    const { page, k8sFrame } = ctx;
    // Add Playwright interactions for this scene ONLY:
    await ctx.hold(1800);
  },
};
`,
  );

  const playlist = await loadPlaylist();
  if (!playlist.includes(id)) {
    const outroIdx = playlist.indexOf("99-outro");
    if (outroIdx >= 0) playlist.splice(outroIdx, 0, id);
    else playlist.push(id);
    savePlaylist(playlist);
  }
  console.log(`Scaffolded ${targetPath} and added '${id}' to playlist.mjs!`);
  console.log(`Preview this scene in ~8s with: make demo_preview SCENE=${id}`);
}

async function cmdToggle(id, enable) {
  const scenesMap = await loadAllScenes();
  if (!scenesMap.has(id)) throw new Error(`Unknown scene '${id}'`);
  let playlist = await loadPlaylist();
  if (enable) {
    if (!playlist.includes(id)) {
      const outroIdx = playlist.indexOf("99-outro");
      if (outroIdx >= 0) playlist.splice(outroIdx, 0, id);
      else playlist.push(id);
    }
  } else {
    playlist = playlist.filter((x) => x !== id);
  }
  savePlaylist(playlist);
  console.log(`${enable ? "Enabled" : "Disabled"} '${id}' in playlist.mjs (${playlist.length} active scenes).`);
  console.log(`Run 'make demo' to re-stitch the final MP4s in < 1s without re-recording other scenes.`);
}

async function buildAndStitch({ mode = "both", targetIds = null, previewOnlyId = null, force = false, voice, outDir }) {
  ensureBinaries();
  mkdirSync(SCENE_CACHE_DIR, { recursive: true });
  mkdirSync(TTS_CACHE_DIR, { recursive: true });
  mkdirSync(outDir, { recursive: true });

  const includeSilent = mode === "both" || mode === "silent";
  const includeNarrated = mode === "both" || mode === "narrated";
  const kubeContext = process.env.KUBE_CONTEXT || "docker-desktop";

  const scenesMap = await loadAllScenes();
  const playlist = previewOnlyId ? [previewOnlyId] : await loadPlaylist();
  for (const id of playlist) {
    if (!scenesMap.has(id)) throw new Error(`Playlist references missing scene '${id}'`);
  }

  const forcedSet = targetIds ? new Set(targetIds.split(",").map((s) => s.trim()).filter(Boolean)) : null;

  // 1. Synthesize / resolve cached neural TTS WAVs for all scenes in playlist (if narrated mode)
  let ttsSegMap = {};
  if (includeNarrated) {
    const py = ensureTtsVenv();
    const prompts = playlist
      .map((id) => scenesMap.get(id).scene)
      .filter((s) => s.voiceover && s.voiceover.trim())
      .map((s) => ({ id: s.id, text: s.voiceover.trim() }));
    const tmpPrompts = join(CACHE_DIR, "prompts.tmp.json");
    const tmpSegs = join(CACHE_DIR, "segments.tmp.json");
    writeFileSync(tmpPrompts, JSON.stringify(prompts, null, 2));
    console.log(`==> Checking neural voiceover cache (${voice})...`);
    execFileSync(py, [join(SCRIPT_DIR, "tts_sync.py"), "synth", tmpPrompts, TTS_CACHE_DIR, tmpSegs, "--voice", voice], {
      stdio: "inherit",
    });
    for (const seg of JSON.parse(readFileSync(tmpSegs, "utf8"))) {
      ttsSegMap[seg.id] = seg;
    }
  }

  // 2. Determine which scenes actually need Playwright UI capture!
  const scenesNeedingCapture = [];
  for (const id of playlist) {
    const info = scenesMap.get(id);
    const rawPath = join(SCENE_CACHE_DIR, id, "raw_timeline.json");
    const forceThis = force || (forcedSet ? forcedSet.has(id) : false);
    if (forceThis || !existsSync(rawPath)) {
      scenesNeedingCapture.push(info);
      continue;
    }
    const raw = JSON.parse(readFileSync(rawPath, "utf8"));
    if (raw.uiHash !== info.uiHash || !raw.frames?.length) {
      scenesNeedingCapture.push(info);
    }
  }

  if (scenesNeedingCapture.length > 0) {
    console.log(
      `==> Recording ${scenesNeedingCapture.length} scene(s) via Playwright: [${scenesNeedingCapture.map((x) => x.scene.id).join(", ")}]`,
    );
    await captureScenes({
      scenesToRecord: scenesNeedingCapture,
      pluginRoot: PLUGIN_ROOT,
      openideaRoot: OPENIDEA_ROOT,
      kubeAppRoot: KUBE_APP_ROOT,
      kubeContext,
      workDir: WORK_DIR,
      sceneCacheRoot: SCENE_CACHE_DIR,
    });
  } else {
    console.log("==> All scene UI frames are up-to-date in cache (0 Playwright runs needed)!");
  }

  // 3. Encode per-scene `clip-silent.mp4` & `clip-narrated.mp4` ONLY for scenes whose UI or voiceover changed!
  const silentClips = [];
  const narratedClips = [];

  for (const id of playlist) {
    const info = scenesMap.get(id);
    const s = info.scene;
    const sDir = join(SCENE_CACHE_DIR, id);
    const framesDir = join(sDir, "frames");
    const raw = JSON.parse(readFileSync(join(sDir, "raw_timeline.json"), "utf8"));
    const metaPath = join(sDir, "clip_meta.json");
    const prevMeta = existsSync(metaPath) ? JSON.parse(readFileSync(metaPath, "utf8")) : {};

    const voiceHash = sha256Short(
      JSON.stringify({
        voice,
        text: s.voiceover || "",
        offset: s.voiceOffsetMs ?? 150,
        tail: s.voiceTailPadMs ?? 480,
      }),
    );

    const silentClipPath = join(sDir, "clip-silent.mp4");
    const narratedClipPath = join(sDir, "clip-narrated.mp4");
    const forceThis = force || (forcedSet ? forcedSet.has(id) : false);

    if (includeSilent) {
      if (forceThis || prevMeta.silentUiHash !== info.uiHash || !existsSync(silentClipPath)) {
        const silentManifest = raw.frames.map((f) => ({
          file: join(framesDir, f.file),
          durationMs: f.silentMs,
        }));
        const mPath = join(sDir, "manifest_silent.json");
        writeFileSync(mPath, JSON.stringify(silentManifest, null, 2));
        console.log(`==> Encoding silent clip [${id}]...`);
        execFileSync(join(BIN_DIR, "encode_mp4"), [mPath, silentClipPath], { stdio: "inherit" });
        prevMeta.silentUiHash = info.uiHash;
      }
      silentClips.push(silentClipPath);
    }

    if (includeNarrated) {
      if (
        forceThis ||
        prevMeta.narratedUiHash !== info.uiHash ||
        prevMeta.voiceHash !== voiceHash ||
        !existsSync(narratedClipPath)
      ) {
        const seg = ttsSegMap[id];
        const voiceOffsetMs = s.voiceOffsetMs ?? 150;
        const voiceTailPadMs = s.voiceTailPadMs ?? 480;
        const neededVoiceMs = seg ? voiceOffsetMs + seg.durationMs + voiceTailPadMs : 0;

        const narratedManifest = raw.frames.map((f) => ({
          file: join(framesDir, f.file),
          durationMs: f.narratedBaseMs,
        }));
        const baseEncodedMs = computeSimulatedEncodedMs(narratedManifest);
        if (neededVoiceMs > baseEncodedMs && narratedManifest.length > 0) {
          narratedManifest[narratedManifest.length - 1].durationMs += neededVoiceMs - baseEncodedMs;
        }
        const finalClipMs = computeSimulatedEncodedMs(narratedManifest);

        const mPath = join(sDir, "manifest_narrated.json");
        const videoOnlyClip = join(sDir, "clip-video-only.mp4");
        const audioOnlyClip = join(sDir, "clip-audio.m4a");
        writeFileSync(mPath, JSON.stringify(narratedManifest, null, 2));

        console.log(`==> Encoding narrated clip [${id}] (${(finalClipMs / 1000).toFixed(2)}s)...`);
        execFileSync(join(BIN_DIR, "encode_mp4"), [mPath, videoOnlyClip], { stdio: "inherit" });
        execFileSync(
          "python3",
          [
            join(SCRIPT_DIR, "tts_sync.py"),
            "clip-audio",
            seg ? seg.wav : "NONE",
            String(finalClipMs),
            String(voiceOffsetMs),
            audioOnlyClip,
          ],
          { stdio: "inherit" },
        );
        execFileSync(join(BIN_DIR, "mux_av"), [videoOnlyClip, audioOnlyClip, narratedClipPath], { stdio: "inherit" });
        prevMeta.narratedUiHash = info.uiHash;
        prevMeta.voiceHash = voiceHash;
      }
      narratedClips.push(narratedClipPath);
    }

    prevMeta.uiHash = info.uiHash;
    writeFileSync(metaPath, JSON.stringify(prevMeta, null, 2));
  }

  // 4. Losslessly stitch all playlist clips via AVAssetExportPresetPassthrough (< 0.5s)!
  if (previewOnlyId) {
    if (includeNarrated && narratedClips.length === 1) {
      const out = join(outDir, `preview-${previewOnlyId}.mp4`);
      execFileSync(join(BIN_DIR, "stitch_clips"), [out, ...narratedClips], { stdio: "inherit" });
    }
    if (includeSilent && silentClips.length === 1) {
      const out = join(outDir, `preview-${previewOnlyId}-silent.mp4`);
      execFileSync(join(BIN_DIR, "stitch_clips"), [out, ...silentClips], { stdio: "inherit" });
    }
    return;
  }

  if (includeSilent) {
    const silentOut = join(outDir, "openidea-kubernetes-demo-1080p-silent.mp4");
    console.log(`==> Stitching ${silentClips.length} silent clips -> ${silentOut}`);
    execFileSync(join(BIN_DIR, "stitch_clips"), [silentOut, ...silentClips], { stdio: "inherit" });
  }
  if (includeNarrated) {
    const narratedOut = join(outDir, "openidea-kubernetes-demo-1080p.mp4");
    console.log(`==> Stitching ${narratedClips.length} narrated clips -> ${narratedOut}`);
    execFileSync(join(BIN_DIR, "stitch_clips"), [narratedOut, ...narratedClips], { stdio: "inherit" });
  }
}

const { positional, flags } = parseFlags(process.argv.slice(2));
const subcmd = positional[0] || "build";
const voice = flags.voice || process.env.VOICE || "en-US-AndrewNeural";
const outDir = resolve(flags["out-dir"] || process.env.DEMO_OUT_DIR || resolve(PLUGIN_ROOT, ".."));
const mode = flags.mode || "both";

if (subcmd === "list") {
  await cmdList();
} else if (subcmd === "new") {
  await cmdNew(positional[1], flags);
} else if (subcmd === "disable") {
  await cmdToggle(positional[1], false);
} else if (subcmd === "enable") {
  await cmdToggle(positional[1], true);
} else if (subcmd === "preview") {
  const id = positional[1] || flags.scene;
  if (!id) throw new Error("Usage: cli.mjs preview <scene-id>");
  await buildAndStitch({ mode, targetIds: flags.force ? id : null, previewOnlyId: id, force: Boolean(flags.force), voice, outDir });
} else if (subcmd === "build") {
  await buildAndStitch({
    mode,
    targetIds: typeof flags.scene === "string" ? flags.scene : null,
    previewOnlyId: null,
    force: Boolean(flags.force),
    voice,
    outDir,
  });
} else {
  throw new Error(`Unknown command '${subcmd}'. Expected: list | build | preview | new | enable | disable`);
}
