OPENIDEA_ROOT ?= $(abspath $(CURDIR)/../openidea)
OPENIDEA_PLUGIN_BIN ?= $(OPENIDEA_ROOT)/target/debug/openidea-plugin
PUBLISHER ?= dengliu
SIGNING_KEY ?= $(HOME)/.openidea/$(PUBLISHER).key
KUBE_CONTEXT ?= docker-desktop
VOICE ?= en-US-AndrewNeural
DEMO_OUT_DIR ?= $(abspath $(CURDIR)/..)

.PHONY: build test check pack demo demo_silent demo_narrated demo_list demo_scene demo_preview demo_new demo_rebuild demo_seed_cluster demo_clean

test:
	pnpm --dir web-src test
	cargo test

build:
	pnpm --dir web-src run build
	"$(OPENIDEA_PLUGIN_BIN)" build .

check:
	"$(OPENIDEA_PLUGIN_BIN)" check .

pack: build
	"$(OPENIDEA_PLUGIN_BIN)" pack . --key "$(SIGNING_KEY)" --publisher "$(PUBLISHER)"

## Incrementally record ONLY new/modified scenes, then stitch silent + narrated 1080p MP4s (< 1s when cached!)
demo:
	OPENIDEA_ROOT="$(OPENIDEA_ROOT)" KUBE_CONTEXT="$(KUBE_CONTEXT)" VOICE="$(VOICE)" DEMO_OUT_DIR="$(DEMO_OUT_DIR)" \
		./scripts/demo/run.sh build --mode both

## Incrementally build & stitch only the silent 1080p MP4
demo_silent:
	OPENIDEA_ROOT="$(OPENIDEA_ROOT)" KUBE_CONTEXT="$(KUBE_CONTEXT)" DEMO_OUT_DIR="$(DEMO_OUT_DIR)" \
		./scripts/demo/run.sh build --mode silent

## Incrementally build & stitch only the neural voiceover 1080p MP4
demo_narrated:
	OPENIDEA_ROOT="$(OPENIDEA_ROOT)" KUBE_CONTEXT="$(KUBE_CONTEXT)" VOICE="$(VOICE)" DEMO_OUT_DIR="$(DEMO_OUT_DIR)" \
		./scripts/demo/run.sh build --mode narrated

## List all modular scenes in scripts/demo/scenes/, their playlist order, and cache status
demo_list:
	./scripts/demo/run.sh list

## Force re-record specific scene(s) (e.g. `make demo_scene SCENE=50-debug-shell`) and re-stitch full MP4s without touching other scenes
demo_scene:
	@test -n "$(SCENE)" || (echo "Usage: make demo_scene SCENE=<scene-id[,id2]>" >&2 && exit 2)
	OPENIDEA_ROOT="$(OPENIDEA_ROOT)" KUBE_CONTEXT="$(KUBE_CONTEXT)" VOICE="$(VOICE)" DEMO_OUT_DIR="$(DEMO_OUT_DIR)" \
		./scripts/demo/run.sh build --mode both --scene "$(SCENE)"

## Preview a single scene standalone in ~8s -> `preview-<SCENE>.mp4` (`make demo_preview SCENE=50-debug-shell`)
demo_preview:
	@test -n "$(SCENE)" || (echo "Usage: make demo_preview SCENE=<scene-id>" >&2 && exit 2)
	OPENIDEA_ROOT="$(OPENIDEA_ROOT)" KUBE_CONTEXT="$(KUBE_CONTEXT)" VOICE="$(VOICE)" DEMO_OUT_DIR="$(DEMO_OUT_DIR)" \
		./scripts/demo/run.sh preview "$(SCENE)" --force

## Scaffold a brand-new scene module (`make demo_new ID=85-port-forward TITLE="One-Click Port Forwarding"`)
demo_new:
	@test -n "$(ID)" || (echo "Usage: make demo_new ID=<scene-id> [TITLE='...']" >&2 && exit 2)
	./scripts/demo/run.sh new "$(ID)" $(if $(TITLE),--title "$(TITLE)",)

## Force clean re-record of ALL enabled scenes in playlist.mjs (keeps neural TTS .wav cache)
demo_rebuild:
	OPENIDEA_ROOT="$(OPENIDEA_ROOT)" KUBE_CONTEXT="$(KUBE_CONTEXT)" VOICE="$(VOICE)" DEMO_OUT_DIR="$(DEMO_OUT_DIR)" \
		./scripts/demo/run.sh build --mode both --force

## Install / refresh sample Helm releases (redis, scylladb, ingress-nginx, node-exporter, podinfo) on $(KUBE_CONTEXT)
demo_seed_cluster:
	KUBE_CONTEXT="$(KUBE_CONTEXT)" ./scripts/demo/seed_cluster.sh

## Remove all cached scene clips, TTS audio, compiled Swift helpers, and scratch workdirs
demo_clean:
	rm -rf scripts/demo/.cache scripts/demo/.work
