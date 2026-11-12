#!/usr/bin/env bash
# Render every site in sites.txt with scout-render: a snapshot (geometry) and a
# screenshot (pixels), both timed. Chromium's half is bench/chrome.mjs.
#
#   bench/run.sh            # all sites
#   bench/run.sh example    # just these
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(dirname "$here")"
bin="$root/target/release/scout-render"
out="$here/results"
UA="Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36"
mkdir -p "$out"

sites=("$@")
if [ ${#sites[@]} -eq 0 ]; then
  mapfile -t sites < <(cut -f1 "$here/sites.txt" | grep -v '^#')
fi

for slug in "${sites[@]}"; do
  url="$(grep -P "^${slug}\t" "$here/sites.txt" | cut -f2)"
  if [ -z "$url" ]; then
    echo "unknown site $slug" >&2
    continue
  fi
  # Fetch the HTML once, then render that exact file. Chromium renders the same
  # bytes, so a page that changes between requests cannot flatter either side.
  curl -sSL --compressed --max-time 30 -A "$UA" "$url" -o "$here/sites/$slug.html"
  # The engine renders the saved file, with --base so its relative stylesheets and
  # images still resolve as they do in the browser.
  start=$(date +%s%N)
  "$bin" snapshot "$here/sites/$slug.html" --base "$url" -o "$out/$slug-ours.json" \
    --width 1280 --height 800 --compact > "$out/$slug-snapshot.log" 2>&1
  snapshot_status=$?
  shot_start=$(date +%s%N)
  "$bin" screenshot "$here/sites/$slug.html" --base "$url" -o "$out/$slug-ours.png" \
    --width 1280 --height 800 > "$out/$slug-screenshot.log" 2>&1
  shot_status=$?
  total=$(( ($(date +%s%N) - start) / 1000000 ))
  shot_ms=$(( ($(date +%s%N) - shot_start) / 1000000 ))
  echo "$slug snapshot=$snapshot_status shot=$shot_status total=${total}ms screenshot=${shot_ms}ms"
done
