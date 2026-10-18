#!/bin/sh
# Assembles the SD card installer site in out/site/ from web/ and the card
# built by scripts/make-sdcard.sh (published to GitHub Pages by CI on a
# version tag):
#   index.html, fonts/       the installer page (web/index.html)
#   release/<path>           every file of the card
#   manifest.json            version, files (size, SHA-256), the paths that
#                            make up the system (backed up before an update)
#                            and the game folders to create
#   usage: scripts/make-site.sh <version>
set -eu
cd "$(dirname "$0")/.."
v=${1:?usage: $0 <version>}
card=out/sdcard
site=out/site
[ -f "$card/cubegm/vmlinux.uImage" ] || { echo "run scripts/make-sdcard.sh first" >&2; exit 1; }
rm -rf "$site"
mkdir -p "$site/release" "$site/fonts"
cp web/index.html "$site/"
for f in ChakraPetch-Bold.ttf ChakraPetch-SemiBold.ttf ChakraPetch-OFL.txt IBMPlexSans-Regular.ttf \
    IBMPlexSans-SemiBold.ttf IBMPlexMono-Regular.ttf IBMPlex-OFL.txt; do
    cp "crates/ui/fonts/$f" "$site/fonts/"
done
# The card's files, without dotfiles or a development agent.conf.
(cd "$card" && find . -type f ! -name '.*' ! -path ./gb350/agent.conf | sed 's|^\./||') |
    while read -r f; do
        mkdir -p "$site/release/$(dirname "$f")"
        cp "$card/$f" "$site/release/$f"
    done
# Written last by the installer: it marks a complete install and its version.
printf '{"version": "%s"}\n' "$v" > "$site/release/gb350/release.json"
python3 - "$site" "$v" <<'PY'
import hashlib, json, os, sys
site, version = sys.argv[1], sys.argv[2]
rel = os.path.join(site, "release")
files = []
for d, _, names in os.walk(rel):
    for n in names:
        p = os.path.join(d, n)
        data = open(p, "rb").read()
        files.append({"path": os.path.relpath(p, rel).replace(os.sep, "/"), "size": len(data),
                      "sha256": hashlib.sha256(data).hexdigest()})
files.sort(key=lambda f: (f["path"] == "gb350/release.json", f["path"]))
manifest = {
    "version": version,
    "system": ["cubegm", "gb350/bin", "gb350/firmware", "gb350/kmod", "gb350/release.json"],
    "folders": ["PS", "SFC", "GBA", "FC", "MD", "PCE", "MAME", "GB", "GBC", "GG", "SMS", "NGPC", "WSC", "ATARI"],
    "files": files,
}
json.dump(manifest, open(os.path.join(site, "manifest.json"), "w"), indent=1)
print(f"{len(files)} files, {sum(f['size'] for f in files) / 1e6:.1f} MB, version {version}")
PY
