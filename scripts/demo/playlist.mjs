/**
 * Ordered playlist of enabled demo scenes.
 *
 * - Remove or comment out any scene ID to exclude it from the final stitched MP4s (< 1s re-stitch!).
 * - Reorder IDs freely — every scene has its own declarative `preset` and cached clips.
 * - Add a new scene with `make demo_new ID=85-port-forward TITLE="One-Click Port Forwarding"`.
 */
export const PLAYLIST = [
  "00-intro",
  "10-yaml-intelligence",
  "15-helm-completion",
  "18-kustomize-overlays",
  "20-cluster-nodes",
  "30-multi-namespace",
  "40-pod-logs",
  "50-debug-shell",
  "60-deploy-spec-apply",
  "70-helm-releases",
  "80-search-crds",
  "99-outro",
];
