#!/bin/bash

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
output="${1:-$repo_root/generated-sources.json}"

# Keep the generator stable so the generated manifest does not change merely
# because flatpak-builder-tools/master changed.
generator_url="https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/de2225a6dee4818c1339b3cdbf29f90c471fcb7e/cargo/flatpak-cargo-generator.py"
generator_dir="$repo_root/target/tools"
generator="$generator_dir/flatpak-cargo-generator.py"

mkdir -p "$generator_dir"
curl -fsSL "$generator_url" -o "$generator"

if command -v uv >/dev/null 2>&1; then
    uv run --quiet --with aiohttp --with tomlkit \
        python "$generator" "$repo_root/Cargo.lock" -o "$output"
elif python3 -c 'import aiohttp, tomlkit' >/dev/null 2>&1; then
    python3 "$generator" "$repo_root/Cargo.lock" -o "$output"
elif flatpak run --command=flatpak-cargo-generator org.flatpak.Builder \
        "$repo_root/Cargo.lock" -o "$output"; then
    :
else
    echo "Cannot run flatpak-cargo-generator." >&2
    echo "Install uv, install Python modules aiohttp and tomlkit, or install org.flatpak.Builder." >&2
    exit 1
fi
