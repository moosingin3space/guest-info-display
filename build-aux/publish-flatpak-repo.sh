#!/bin/bash
# Assemble the Flatpak repository site: the live repository plus new bundles.
#
#   publish-flatpak-repo.sh SITE_DIR BUNDLE...
#
# GitHub Pages replaces the whole site on every deploy, so the live site is
# the only copy of the repository. This mirrors it back down, commits each
# bundle on top of its branch (so clients get small static deltas), prunes
# old commits, and writes the .flatpakrepo/.flatpakref files and index page.
# A bundle whose content matches its branch head is skipped, so passing an
# already-published bundle again is harmless.
#
# Environment:
#   SITE_URL     where the site is served, e.g. https://user.github.io/repo
#   GPG_KEY_ID   key that signs commits and the summary
#   GPG_HOMEDIR  keyring holding it (default: gpg's own)
#   PRUNE_DEPTH  commits of history kept per branch (default: 5)
set -euo pipefail

site=${1:?usage: $0 SITE_DIR BUNDLE...}
shift
[[ $# -gt 0 ]] || { echo "no bundles given" >&2; exit 1; }
: "${SITE_URL:?}" "${GPG_KEY_ID:?}"
SITE_URL=${SITE_URL%/}
prune_depth=${PRUNE_DEPTH:-5}

gpg_args=(--gpg-sign="$GPG_KEY_ID")
gpg_export=(gpg)
if [[ -n ${GPG_HOMEDIR:-} ]]; then
    gpg_args+=(--gpg-homedir="$GPG_HOMEDIR")
    gpg_export+=(--homedir "$GPG_HOMEDIR")
fi

repo=$site/repo
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

rm -rf "$site"
mkdir -p "$site"
"${gpg_export[@]}" --export "$GPG_KEY_ID" >"$work/key.gpg"
[[ -s $work/key.gpg ]] || { echo "GPG key $GPG_KEY_ID not found" >&2; exit 1; }
ostree init --repo="$repo" --mode=archive-z2

# Only a 404 means "no repository yet". Anything else could be a transient
# failure, and starting fresh then would drop every published build.
status=$(curl -sS -o /dev/null -w '%{http_code}' "$SITE_URL/repo/summary")
case $status in
    200)
        echo "Mirroring the live repository from $SITE_URL/repo"
        ostree remote add --repo="$repo" --gpg-import="$work/key.gpg" \
            --set=gpg-verify-summary=true live "$SITE_URL/repo"
        ostree pull --repo="$repo" --mirror --depth=-1 live
        ostree remote delete --repo="$repo" live
        ;;
    404)
        echo "No repository at $SITE_URL/repo yet; starting a new one"
        ;;
    *)
        echo "Unexpected HTTP $status fetching $SITE_URL/repo/summary" >&2
        exit 1
        ;;
esac

# build-import-bundle would install each bundle's parentless commit as the
# branch head; build-commit-from recommits it on top of the existing head.
for bundle in "$@"; do
    staging=$work/staging
    rm -rf "$staging"
    ostree init --repo="$staging" --mode=archive-z2
    flatpak build-import-bundle --no-update-summary "$staging" "$bundle"
    for ref in $(ostree refs --repo="$staging"); do
        echo "Committing $ref from ${bundle##*/}"
        flatpak build-commit-from --no-update-summary "${gpg_args[@]}" \
            --src-repo="$staging" --subject="Build of $ref from ${bundle##*/}" \
            "$repo" "$ref"
    done
done

flatpak build-update-repo "${gpg_args[@]}" \
    --title="Guest Info Display" \
    --generate-static-deltas --prune --prune-depth="$prune_depth" \
    "$repo"

# ---------------------------------------------------------------------------
# Install files and the landing page.
# ---------------------------------------------------------------------------

key_b64=$(base64 -w0 "$work/key.gpg")
cp "$work/key.gpg" "$site/guest-info-display.gpg"

cat >"$site/guest-info-display.flatpakrepo" <<EOF
[Flatpak Repo]
Title=Guest Info Display
Url=$SITE_URL/repo/
Homepage=https://github.com/moosingin3space/guest-info-display
Comment=Builds of Guest Info Display
GPGKey=$key_b64
EOF

refs=$(ostree refs --repo="$repo")
listing=""
for app in xyz.mooshq.GuestInfoDisplay:stable xyz.mooshq.GuestInfoDisplay.Devel:master; do
    id=${app%%:*}
    branch=${app##*:}
    grep -q "^app/$id/[^/]*/$branch\$" <<<"$refs" || continue

    title="Guest Info Display"
    [[ $id == *.Devel ]] && title="$title (Devel)"
    cat >"$site/$id.flatpakref" <<EOF
[Flatpak Ref]
Name=$id
Branch=$branch
Title=$title
Url=$SITE_URL/repo/
SuggestRemoteName=guest-info-display
RuntimeRepo=https://dl.flathub.org/repo/flathub.flatpakrepo
IsRuntime=false
GPGKey=$key_b64
EOF

    ref=$(grep -m1 "^app/$id/[^/]*/$branch\$" <<<"$refs")
    commit=$(ostree rev-parse --repo="$repo" "$ref")
    date=$(ostree show --repo="$repo" "$commit" | sed -n 's/^Date: *//p')
    listing+="<h2>$title</h2>
<p><code>$id//$branch</code> &middot; commit <code>${commit:0:12}</code> &middot; $date</p>
<pre>flatpak install --user $SITE_URL/$id.flatpakref</pre>
"
done

cat >"$site/index.html" <<EOF
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Guest Info Display Flatpak repository</title>
<style>
  :root { color-scheme: light dark; }
  body { font: 16px/1.5 system-ui, sans-serif; max-width: 44rem; margin: 2rem auto; padding: 0 1rem; }
  pre { padding: .75rem; overflow-x: auto; border-radius: 6px; background: color-mix(in srgb, currentColor 8%, transparent); }
</style>
</head>
<body>
<h1>Guest Info Display</h1>
<p>Flatpak builds of <a href="https://github.com/moosingin3space/guest-info-display">guest-info-display</a>.
Tagged releases are published as <code>xyz.mooshq.GuestInfoDisplay</code>; every push to
<code>main</code> is published as <code>xyz.mooshq.GuestInfoDisplay.Devel</code>. Both can be installed side by side.</p>
<p>To add the repository without installing anything:</p>
<pre>flatpak remote-add --user --if-not-exists guest-info-display $SITE_URL/guest-info-display.flatpakrepo</pre>
$listing
</body>
</html>
EOF

echo "Site assembled in $site:"
ostree refs --repo="$repo"
du -sh "$repo"
