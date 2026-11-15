# OpenIDEA Kubernetes 1080p Demo Video Pipeline

Modular, incremental 1080p (`1920 × 1080`, 30 fps, H.264 + 48 kHz Stereo AAC) showcase video generator for `dengliu.kubernetes`, producing **both** a brisk **silent MP4** and a voice-synchronized **neural-narrated MP4**.

---

## 1. Architecture (`Isolate → Two-Tier Cache → Lossless Stitch`)

Instead of a monolithic script where earlier scenes leak UI state into later scenes, each scene is compiled into an independent `.mp4` clip and concatenated via macOS `AVFoundation` passthrough (`AVAssetExportPresetPassthrough`) with **zero re-encoding** in `< 0.5s`:

```text
scripts/demo/
├── playlist.mjs              # Ordered list of active scene IDs (reorder / disable / delete 1 line)
├── cli.mjs                   # Incremental build, preview, scaffold, list, enable/disable CLI
├── harness.mjs               # Playwright + openidea-server --stdio + declarative `applyPreset()`
├── stitch_clips.swift        # Lossless AVFoundation passthrough MP4+AAC concatenator (< 0.5s)
├── encode_mp4.swift          # Per-scene 1080p H.264 encoder
├── mux_av.swift              # Per-scene H.264 + 48 kHz AAC muxer
├── tts_sync.py               # SHA-256 cached neural TTS + sample-accurate per-clip audio pad
├── seed_cluster.sh           # Idempotent cluster seeder (simple-app, podinfo, redis, scylladb, ingress-nginx, ...)
├── run.sh                    # Environment/PATH wrapper invoked by ../../Makefile
├── fixtures/kube-app/        # Self-contained Kubernetes, Helm & Kustomize demo workspace
└── scenes/                   # Self-contained, order-independent scene modules
    ├── 00-intro.mjs
    ├── 10-yaml-intelligence.mjs
    ├── 15-helm-completion.mjs
    ├── 18-kustomize-overlays.mjs
    ├── 20-cluster-nodes.mjs
    ├── 30-multi-namespace.mjs
    ├── 40-pod-logs.mjs
    ├── 50-debug-shell.mjs
    ├── 60-deploy-spec-apply.mjs
    ├── 70-helm-releases.mjs
    ├── 80-search-crds.mjs
    └── 99-outro.mjs
```

### Why It Stays Fast & Repeatable
- **Declarative `preset` Preconditions:** Before frame 1 of *any* scene, [harness.mjs](./harness.mjs) normalizes the IDE and Cluster Explorer (`projectSidebar`, `closeAllEditorTabs`, `explorerOpen`, `rightWidth`, `bottomHeight`, `namespace`, `explorerKind`, `openFile`, `drawerClosed`), so any scene runs identically whether standalone or in sequence.
- **Semantic Category Badges:** Callout pills use semantic labels (e.g., `POD & EPHEMERAL DEBUG SHELL`) rather than `"SCENE 5 / 8"`, so inserting, deleting, or reordering scenes in [playlist.mjs](./playlist.mjs) never invalidates other scenes' cached frames or clips.
- **Two-Tier Content-Addressed Cache (`.cache/scenes/<id>/`):**
  - **`uiHash`** tracks `{ preset, badge, title, subtitle, calloutPos, record.toString() }`.
  - **`voiceHash`** tracks `{ voice, voiceover, voiceOffsetMs, voiceTailPadMs }`.
  - Editing **only `voiceover`** skips Playwright completely (`uiHash` cache hit), synthesizes only the 1 updated `.wav`, adjusts the final resting frame's hold duration, re-encodes that 1 clip (~1s), and stitches (< 0.5s).
  - Editing **`playlist.mjs`** (delete/disable/reorder) skips Playwright, TTS, and H.264 encoding — re-stitching both 1080p MP4s in **~0.3s**.

---

## 2. Quick Start & `Makefile` Commands

Run from the repository root (`openidea-kubernetes/`), defined in [Makefile](../../Makefile):

```sh
# 0. Build latest webview + Wasm component (and seed sample cluster workloads if needed)
make build
make demo_seed_cluster

# 1. Inspect all scenes, playlist order, and cache status (`up-to-date` vs `modified` vs `missing`)
make demo_list

# 2. Incremental build: records ONLY new/modified scenes, reuses cached clips, and stitches BOTH MP4s
make demo

# 3. Build only the silent cut OR only the narrated cut
make demo_silent
make demo_narrated VOICE=en-US-AndrewNeural   # also: en-US-AvaNeural, en-US-BrianNeural

# 4. Scaffold a brand-new scene module + insert before `99-outro` in playlist.mjs
make demo_new ID=55-port-forward TITLE="One-Click Localhost Port Forwarding"

# 5. Fast ~8s standalone preview of 1 scene while iterating -> `preview-<SCENE>.mp4`
make demo_preview SCENE=55-port-forward

# 6. Force re-record specific scene(s) and re-stitch full MP4s without touching other scenes
make demo_scene SCENE=50-debug-shell

# 7. Disable / re-enable a scene (< 1s lossless re-stitch, 0 Playwright runs)
./scripts/demo/run.sh disable 30-multi-namespace && make demo
./scripts/demo/run.sh enable  30-multi-namespace && make demo

# 8. Force clean re-record of ALL enabled scenes (keeps cached neural TTS .wav files)
make demo_rebuild

# 9. Wipe all cached clips, TTS audio, and compiled Swift helpers
make demo_clean
```

### Environment Overrides
- `KUBE_CONTEXT` — target kubeconfig context (default: `docker-desktop`)
- `VOICE` — Microsoft Edge Neural TTS voice (default: `en-US-AndrewNeural`; alternatives: `en-US-AvaNeural`, `en-US-BrianNeural`)
- `DEMO_OUT_DIR` — output folder for `openidea-kubernetes-demo-1080p.mp4` and `openidea-kubernetes-demo-1080p-silent.mp4` (default: parent directory `..`)
- `KUBE_APP_ROOT` — demo workspace directory copied into the clean capture workdir (default: `scripts/demo/fixtures/kube-app`)
- `OPENIDEA_ROOT` — sibling `openidea` checkout path (default: `../openidea`)

---

## 3. Scene Module Contract (`scenes/<id>.mjs`)

See [scenes/50-debug-shell.mjs](./scenes/50-debug-shell.mjs) or [scenes/60-deploy-spec-apply.mjs](./scenes/60-deploy-spec-apply.mjs) for complete examples:

```js
export default {
  id: "55-port-forward",
  badge: "ONE-CLICK PORT FORWARDING",   // Semantic badge (never hardcode "SCENE X / N")
  title: "Forward Container & Service Ports in One Click",
  subtitle: "Click any container port in the Pod drawer to open a live localhost tunnel.",
  calloutPos: "top-left",               // "bottom-left" | "bottom-right" | "top-left"
  voiceover: "Need to test an internal service locally? Click any container port to open a live port-forward tunnel in Open IDEA.",
  preset: {
    hero: false,
    projectSidebar: false,
    closeAllEditorTabs: true,
    explorerOpen: true,
    rightWidth: 760,
    bottomHeight: 340,
    terminalOpen: false,
    drawerClosed: true,
    namespace: "default",               // "all" | "<namespace>"
    explorerKind: "Pod",                // "Overview" | "Node" | "Pod" | "Deployment" | "HelmRelease" | "CustomResourceDefinition"
  },
  async record(ctx) {
    // Only the Playwright actions for THIS scene.
    // Narrated tail hold padding on the final frame is computed automatically from `voiceover` duration.
    await ctx.hold(1500);
  },
  async cleanup(ctx) {
    // Optional cluster cleanup (e.g., ctx.scaleDeployment("podinfo", "default", 1))
  },
};
```

---

## 4. Prompting an AI Agent to Add or Customize Scenes

With the repo's [record-k8s-demo](../../.github/skills/record-k8s-demo/SKILL.md) skill, you only need to specify **what you want viewers to see and hear**. Any knob you omit uses sensible defaults:

| What to Customize | Scene Field | Example Phrasing in Prompt |
| --- | --- | --- |
| **1. Placement / Order** | Position in [playlist.mjs](./playlist.mjs) | *"Insert a new scene `55-port-forward` right after `50-debug-shell`"* |
| **2. Starting View & Target** | `preset` (`namespace`, `explorerKind`, `openFile`, `rightWidth`) | *"Start in namespace `cache` on `Pods` with the Project sidebar closed"* |
| **3. Exact Clicks / Commands** | `record(ctx)` | *"Open `redis-master-0`, click Port Forward on `6379`, and show the Terminal tab"* |
| **4. Callout Card Text / Position** | `badge`, `title`, `subtitle`, `calloutPos` | *"Title: `One-Click Port Forwarding`, put the callout at `top-left`"* |
| **5. Voiceover Script / Voice** | `voiceover` + `VOICE` | *"Say: 'Click any container port to tunnel traffic to localhost in one click.'"* |

### Copy-Paste Prompt Templates

#### A. Add a new customized scene (fast 8s preview first)
> Add a new demo scene `55-port-forward` after `50-debug-shell`:
> - Open pod `podinfo` in `default`, scroll to Containers → Ports in the drawer, and click `9898/TCP` to start port-forwarding in the native Terminal tab.
> - Put the callout card at `top-left` with title `"One-Click Localhost Port Forwarding"`.
> - Voiceover: `"Need to test an internal service locally? Click any container or service port to open a live port-forward tunnel in OpenIDEA's Terminal."`
> - Run `make demo_preview` first so I can review `preview-55-port-forward.mp4`.

*(Once happy with the preview: **"Looks good, stitch it into the full demo"** → `make demo` stitches in `< 1s`.)*

#### B. Showcase a newly built plugin feature
> We just added custom Helm & Kustomize folder icons in `0.3.7`. Rebuild the plugin, add a short scene `05-custom-icons` right after `00-intro` that expands `charts/podinfo` and `overlays/prod` in the Project tree to highlight the new icons, and update both demo MP4s.

#### C. Tweak one existing scene without touching any other scene
> Update scene `50-debug-shell` only: instead of `podinfo`, attach the custom debug shell to `redis-master-0` in namespace `cache`, run `ps aux` and `ss -tulpn` in the terminal, update the voiceover to mention Redis, and re-build the demo videos without re-recording the other scenes.

#### D. Tweak only the voiceover or callout text (~3s rebuild, 0 Playwright runs)
> In scene `70-helm-releases`, change the voiceover to: `"Manage all your Helm deployments across namespaces — from Redis and ScyllaDB to Ingress NGINX — with instant access to release notes and custom values."` Re-stitch the narrated video.

---

## 5. UI & Neural TTS Invariants

1. **Neural TTS Phonetics:** Always write `"Open IDEA"` (with a space) and `"Scylla D B"` inside `voiceover` strings (`edge-tts` drops mixed-case `"OpenIDEA"` with `NoAudioReceived`). Keep `"OpenIDEA"` / `"ScyllaDB"` in visual `title` / `subtitle` strings.
2. **Unclipped Native Editor Toolbars (`Reload` / `Diff` / `Save & Apply`):** Set `projectSidebar: false` and `rightWidth: 760` in `preset` so the left Native Editor pane gets `~1,080 px` width and toolbar buttons never clip.
3. **Table Row Clicks (`offsetX: 110`):** Always pass `{ offsetX: 110 }` when clicking `.fl-table-row` / `tr` to target the `Name` column rather than clickable `Namespace` / `Controlled By` links.
4. **Drawer Close (`closeExplorerDrawer()`):** Always close the Explorer Drawer via `await ctx.closeExplorerDrawer()` (`dispatchEvent("click")`) rather than a raw top-right mouse click so OpenIDEA's floating `<ToolWindowCornerActions>` `Hide` button is never hit.
5. **Callout Card Position:** Use `"top-left"` when the bottom native `Terminal` tool window is open, `"bottom-right"` when highlighting the left Native Editor, and `"bottom-left"` when highlighting the right Cluster Explorer.
