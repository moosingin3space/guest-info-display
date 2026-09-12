# Guest Info Display task runner. Run `just` to list recipes.

set shell := ["bash", "-euo", "pipefail", "-c"]

app_id := "xyz.mooshq.GuestInfoDisplay"
manifest := app_id + ".json"
bundle := "guest-info-display.flatpak"
sdk := "org.freedesktop.Sdk//25.08"
root := justfile_directory()
generator_url := "https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/master/cargo/flatpak-cargo-generator.py"

# Settings database for `just run`, kept apart from the installed Flatpak's.
dev_data := root / "target/dev-data"

default:
    @just --list

# ---------------------------------------------------------------------------
# Development — inside the 25.08 SDK, so the host needs no Skia link deps.
# ---------------------------------------------------------------------------

# Run cargo inside the Flatpak SDK, e.g. `just cargo clippy`
cargo *args:
    flatpak run --user --devel --share=network \
        --filesystem={{root}} \
        --env=CARGO_TARGET_DIR={{root}}/target/sdk \
        --command=bash {{sdk}} \
        -c 'export PATH=/usr/lib/sdk/rust-stable/bin:$PATH; cd {{root}} && cargo "$@"' cargo {{args}}

# Debug build
build:
    just cargo build

# Run the test suite
test:
    just cargo test

# Build and launch the debug binary in the SDK (RUST_LOG is passed through)
run: build
    mkdir -p {{dev_data}}
    flatpak run --user --devel --share=network --share=ipc \
        --socket=wayland --socket=fallback-x11 --socket=pulseaudio --device=dri \
        --filesystem={{root}} \
        --env=XDG_DATA_HOME={{dev_data}} \
        --env=RUST_LOG="${RUST_LOG:-guest_info_display=info}" \
        --command={{root}}/target/sdk/debug/guest-info-display {{sdk}}

# Regenerate generated-sources.json from Cargo.lock
sources: _generator
    uv run --quiet --with aiohttp --with tomlkit \
        python target/tools/flatpak-cargo-generator.py Cargo.lock -o generated-sources.json

# ---------------------------------------------------------------------------
# CI — the jobs in .github/workflows/ci.yml, run locally.
# ---------------------------------------------------------------------------

# Run every CI job locally
ci: check-sources ci-build flatpak-bundle

# CI "check-generated-sources": generated-sources.json must match Cargo.lock
check-sources: _generator
    uv run --quiet --with aiohttp --with tomlkit \
        python target/tools/flatpak-cargo-generator.py Cargo.lock -o target/generated-sources.json
    @if ! diff -q generated-sources.json target/generated-sources.json >/dev/null; then \
        echo "generated-sources.json is out of sync with Cargo.lock; run 'just sources'."; \
        exit 1; \
    fi
    @echo "generated-sources.json is up to date."

# CI "Build and test": the workflow's own Wolfi packages and command, in rootless podman
ci-build:
    #!/usr/bin/env -S uv run --quiet --script
    # /// script
    # dependencies = ["pyyaml"]
    # ///
    # Read the step from the workflow rather than duplicating it, so this
    # can't drift from what CI runs.
    import os, shlex, subprocess, sys, yaml

    workflow = yaml.safe_load(open(".github/workflows/ci.yml"))
    step = next(
        s for s in workflow["jobs"]["build"]["steps"] if "wolfi-act" in s.get("uses", "")
    )
    packages = step["with"]["packages"].replace(",", " ")
    command = step["with"]["command"]
    script = f"apk add --no-cache bash {packages} && exec bash -ec {shlex.quote(command)}"
    result = subprocess.run(
        [
            "podman", "run", "--rm",
            "--security-opt", "label=disable",
            "-v", f"{os.getcwd()}:/work",
            "-w", "/work",
            # Keep Wolfi's artifacts apart from the SDK build's.
            "-e", "CARGO_TARGET_DIR=/work/target/wolfi",
            "cgr.dev/chainguard/wolfi-base",
            "sh", "-c", script,
        ],
    )
    sys.exit(result.returncode)

# CI "Build Flatpak": offline flatpak-builder build, exported as a bundle
flatpak-bundle arch=`uname -m`:
    flatpak run org.flatpak.Builder --user --force-clean --arch={{arch}} \
        --install-deps-from=flathub --repo=repo builddir {{manifest}}
    flatpak build-bundle --arch={{arch}} repo {{bundle}} {{app_id}}

# ---------------------------------------------------------------------------
# Trying the Flatpak on this machine.
# ---------------------------------------------------------------------------

# Install the bundle into your user installation, replacing any earlier one
flatpak-install: flatpak-bundle
    flatpak install --user --reinstall --noninteractive --bundle {{bundle}}

# Launch the installed Flatpak (RUST_LOG is passed through)
flatpak-run:
    flatpak run --env=RUST_LOG="${RUST_LOG:-guest_info_display=info}" {{app_id}}

# Build, install and launch the Flatpak
flatpak-test: flatpak-install flatpak-run

# Remove the installed Flatpak (keeps its settings in ~/.var/app)
flatpak-uninstall:
    flatpak uninstall --user --noninteractive {{app_id}}

# Delete Flatpak build output and the bundle
clean:
    rm -rf builddir repo .flatpak-builder {{bundle}}

_generator:
    mkdir -p target/tools
    curl -fsSL {{generator_url}} -o target/tools/flatpak-cargo-generator.py
