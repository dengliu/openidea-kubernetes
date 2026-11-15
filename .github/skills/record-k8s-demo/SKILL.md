---
name: record-k8s-demo
description: Record, incrementally update, add, tweak, reorder, or delete scenes in the 1080p silent and neural-narrated showcase MP4 videos for the OpenIDEA Kubernetes plugin (`dengliu.kubernetes`). Triggers on "record demo video", "re-record k8s demo", "update showcase video", "add a demo scene", "tweak scene", "delete scene", "record silent and narrated mp4".
---

# Modular Scene-Clip Demo Pipeline (`dengliu.kubernetes`)

Each demo scene is an **isolated, self-contained module** (`scripts/demo/scenes/<id>.mjs`) with its own declarative IDE/Explorer `preset`, SHA-256 content-addressed frame & TTS cache (`scripts/demo/.cache/scenes/<id>/`), and pre-encoded H.264/AAC clips (`clip-silent.mp4` + `clip-narrated.mp4`). Final deliverables are assembled in **< 0.5s** with zero re-encoding via macOS `AVFoundation` passthrough stitching (`stitch_clips.swift`).

## Common Workflows (`Makefile` & `cli.mjs`)

Run from `/Users/dengliu/git/openidea_all/openidea-kubernetes`:

```sh
# 1. Inspect all scenes, playlist order, and cache status (`up-to-date` vs `modified` vs `missing`)
make demo_list

# 2. Incremental build: records ONLY new/modified scenes, reuses cached clips for all untouched scenes, and stitches both MP4s!
make demo

# 3. Tweak / re-record ONE specific scene without touching any other scene (~10s total):
make demo_scene SCENE=50-debug-shell

# 4. Fast standalone preview of 1 scene while developing -> `preview-<id>.mp4` (~8s):
make demo_preview SCENE=85-port-forward

# 5. Scaffold a brand-new scene file + insert into playlist.mjs before `99-outro`:
make demo_new ID=85-port-forward TITLE="One-Click Port Forwarding"

# 6. Disable / delete or re-enable a scene (< 1s lossless re-stitch, 0 Playwright runs!):
./scripts/demo/run.sh disable 30-multi-namespace && make demo
./scripts/demo/run.sh enable  30-multi-namespace && make demo

# 7. Full clean re-record of all enabled scenes (e.g. after a major UI theme/layout overhaul):
make demo_rebuild
```

## Scene Module Contract (`scripts/demo/scenes/<id>.mjs`)

Every scene file exports a default object:

```js
export default {
  id: "85-port-forward",
  badge: "ONE-CLICK PORT FORWARDING", // Semantic category pill (never hardcode "SCENE X / N"!)
  title: "Forward Container & Service Ports in One Click",
  subtitle: "Click any container port in the Pod drawer to open a live localhost tunnel.",
  calloutPos: "bottom-left",          // "bottom-left" | "bottom-right" | "top-left"
  voiceover: "Click any container port to start a local port-forward session in Open IDEA.",
  preset: {
    hero: false,
    projectSidebar: false,
    closeAllEditorTabs: true,
    explorerOpen: true,
    rightWidth: 1220,
    terminalOpen: false,
    drawerClosed: true,
    namespace: "default",             // "all" | "<namespace>"
    explorerKind: "Pod",              // "Overview" | "Node" | "Pod" | "Deployment" | "HelmRelease" | "CustomResourceDefinition"
  },
  async record(ctx) {
    // Only the Playwright steps for THIS scene! Voiceover hold padding on the final frame is automatic.
    await ctx.hold(1500);
  },
  async cleanup(ctx) {
    // Optional teardown (e.g. restore scaled replicas)
  },
};
```

## Two-Tier Cache & Critical Invariants

1. **Two-Tier Cache (`uiHash` vs `voiceHash`)**:
   - Editing **only `voiceover`** does **not** change `uiHash`: `cli.mjs` skips Playwright completely, synthesizes only the 1 updated `.wav`, adjusts the final frame's hold duration, re-encodes only that scene's `clip-narrated.mp4` (~1s), and passthrough-stitches (< 0.5s).
   - Editing **`playlist.mjs`** (reordering or deleting scenes) changes neither hash: `make demo` stitches existing clips in **< 0.5s**.
2. **No Hardcoded Scene Numbers in Badges**: Keep `badge` semantic (e.g. `"HELM RELEASE INSPECTOR"`), never `"SCENE 7 / 8"`, so inserting/deleting a scene never invalidates other scenes' cached frames.
3. **TTS Phonetics**: Always spell `"OpenIDEA"` as `"Open IDEA"` (with a space) and `"ScyllaDB"` as `"Scylla D B"` in `voiceover` (`edge-tts` drops mixed-case `"OpenIDEA"` with `NoAudioReceived`).
4. **Unclipped Native Editor Toolbars (`Reload` / `Diff` / `Save & Apply`)**: Set `projectSidebar: false` and `rightWidth: 760` in `preset` so the left Native Editor gets `~1,080 px` width and buttons never clip.
5. **Table Row Clicks (`offsetX: 110`) & Drawer Close**: Always pass `{ offsetX: 110 }` when clicking `.fl-table-row` / `tr` (avoids `Namespace` / `Controlled By` filter links), and always use `await ctx.closeExplorerDrawer()` (`dispatchEvent("click")`) rather than raw top-right viewport mouse clicks.
