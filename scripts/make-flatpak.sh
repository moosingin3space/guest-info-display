#!/bin/bash

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"

"$repo_root/scripts/generate-flatpak-sources.sh"
flatpak run --command=flathub-build org.flatpak.Builder ./xyz.mooshq.GuestInfoDisplay.json "$@"
