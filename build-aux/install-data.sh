#!/bin/sh
# Install the desktop file, metainfo and icon under ${FLATPAK_ID}, so the
# same sources serve both xyz.mooshq.GuestInfoDisplay and its .Devel variant.
set -eu

base_id=xyz.mooshq.GuestInfoDisplay
name="Guest Info Display"
case "$FLATPAK_ID" in
    *.Devel) name="$name (Devel)" ;;
esac

# Rewrites the app ID, and the display name wherever it stands alone.
rewrite() {
    sed -e "s/$base_id/$FLATPAK_ID/g" \
        -e "s|^Name=Guest Info Display\$|Name=$name|" \
        -e "s|<name>Guest Info Display</name>|<name>$name</name>|" \
        "$1"
}

install -d "$FLATPAK_DEST/share/applications" "$FLATPAK_DEST/share/metainfo" \
    "$FLATPAK_DEST/share/icons/hicolor/scalable/apps"
rewrite "data/$base_id.desktop" >"$FLATPAK_DEST/share/applications/$FLATPAK_ID.desktop"
rewrite "data/$base_id.metainfo.xml" >"$FLATPAK_DEST/share/metainfo/$FLATPAK_ID.metainfo.xml"
install -m0644 "data/$base_id.svg" "$FLATPAK_DEST/share/icons/hicolor/scalable/apps/$FLATPAK_ID.svg"
