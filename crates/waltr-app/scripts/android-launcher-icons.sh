#!/usr/bin/env bash
set -euo pipefail

app_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
crates_dir=$(cd "$app_dir/.." && pwd)
favicon="$crates_dir/waltr-web/public/favicon.svg"
icons="$app_dir/icons"
packaged="$app_dir/gen/android/app/src/main/res"
duplicate="$icons/android"
densities=(mdpi hdpi xhdpi xxhdpi xxxhdpi)
legacy_sizes=(48 72 96 144 192)
foreground_sizes=(108 162 216 324 432)
mode=${1:-generate}

if [[ $mode != generate && $mode != --check ]] || [[ $# -gt 1 ]]; then
  printf 'usage: %s [--check]\n' "$0" >&2
  exit 2
fi

command -v perl >/dev/null || { printf 'perl is required\n' >&2; exit 1; }
command -v rsvg-convert >/dev/null || { printf 'rsvg-convert is required\n' >&2; exit 1; }
command -v magick >/dev/null || { printf 'ImageMagick is required\n' >&2; exit 1; }

generated=$(mktemp -d)
trap 'rm -rf "$generated"' EXIT
mkdir -p "$generated/native" "$generated/res/mipmap-anydpi-v26" "$generated/res/values"

cp "$favicon" "$generated/native/icon.svg"
perl -0pe '
  s/ role="img" aria-labelledby="title desc"//;
  s/\n  <title[^>]*>.*?<\/title>//;
  s/\n  <desc[^>]*>.*?<\/desc>//;
  s/\n  <rect[^>]* \/>//g;
' "$generated/native/icon.svg" > "$generated/native/icon-foreground.svg"
perl -0pe '
  s/ role="img" aria-labelledby="title desc"//;
  s/\n  <title[^>]*>.*?<\/title>//;
  s/\n  <desc[^>]*>.*?<\/desc>//;
  s{  <defs>}{  <defs>\n    <clipPath id="circle"><circle cx="256" cy="256" r="256" /></clipPath>};
  s{  <rect width="512" height="512" rx="112" fill="#05040a" />}{  <g clip-path="url(#circle)">\n    <rect width="512" height="512" fill="#05040a" />};
  s{<rect x="18" y="18" width="476" height="476" rx="104"}{<rect x="18" y="18" width="476" height="476"}g;
  s{  <rect x="30" y="30" width="452" height="452" rx="92" fill="none" stroke="#f6f1ff" stroke-opacity="0.13" stroke-width="4" />}{    <circle cx="256" cy="256" r="226" fill="none" stroke="#f6f1ff" stroke-opacity="0.13" stroke-width="4" />};
  s{  </g>\n</svg>\n\z}{  </g>\n  </g>\n</svg>\n};
' "$generated/native/icon.svg" > "$generated/native/icon-round.svg"

rsvg-convert --width 512 --height 512 "$generated/native/icon.svg" \
  --output "$generated/native/icon.png"

for index in "${!densities[@]}"; do
  density=${densities[$index]}
  legacy_size=${legacy_sizes[$index]}
  foreground_size=${foreground_sizes[$index]}
  output="$generated/res/mipmap-$density"
  mkdir -p "$output"
  rsvg-convert --width "$legacy_size" --height "$legacy_size" "$generated/native/icon.svg" \
    --output "$output/ic_launcher.png"
  rsvg-convert --width "$legacy_size" --height "$legacy_size" "$generated/native/icon-round.svg" \
    --output "$output/ic_launcher_round.png"
  rsvg-convert --width "$foreground_size" --height "$foreground_size" "$generated/native/icon-foreground.svg" \
    --output "$output/ic_launcher_foreground.png"
done

printf '%s\n' \
  '<?xml version="1.0" encoding="utf-8"?>' \
  '<adaptive-icon xmlns:android="http://schemas.android.com/apk/res/android">' \
  '  <background android:drawable="@color/ic_launcher_background" />' \
  '  <foreground android:drawable="@mipmap/ic_launcher_foreground" />' \
  '</adaptive-icon>' \
  > "$generated/res/mipmap-anydpi-v26/ic_launcher.xml"
cp "$generated/res/mipmap-anydpi-v26/ic_launcher.xml" \
  "$generated/res/mipmap-anydpi-v26/ic_launcher_round.xml"
printf '%s\n' \
  '<?xml version="1.0" encoding="utf-8"?>' \
  '<resources>' \
  '  <color name="ic_launcher_background">#100c1d</color>' \
  '</resources>' \
  > "$generated/res/values/ic_launcher_background.xml"

for index in "${!densities[@]}"; do
  density=${densities[$index]}
  expected_legacy=${legacy_sizes[$index]}
  expected_foreground=${foreground_sizes[$index]}
  for name in ic_launcher ic_launcher_round; do
    actual=$(magick identify -format '%wx%h' "$generated/res/mipmap-$density/$name.png")
    [[ $actual == "${expected_legacy}x${expected_legacy}" ]]
  done
  foreground="$generated/res/mipmap-$density/ic_launcher_foreground.png"
  actual=$(magick identify -format '%wx%h' "$foreground")
  [[ $actual == "${expected_foreground}x${expected_foreground}" ]]
  alpha_min=$(magick "$foreground" -alpha extract -format '%[min]' info:)
  alpha_max=$(magick "$foreground" -alpha extract -format '%[max]' info:)
  [[ $alpha_min == 0 && $alpha_max == 65535 ]]

  for image in "$generated/res/mipmap-$density"/ic_launcher*.png; do
    lime=$(magick "$image" -alpha off -fuzz 8% -fill black +opaque '#d7ff73' \
      -fill white -opaque '#d7ff73' -format '%[fx:mean]' info:)
    [[ $lime == 0 ]]
  done
done

if grep -E -i '#(17211b|d7ff73)|lime|green' "$generated/native"/*.svg >/dev/null; then
  printf 'legacy green/lime launcher palette found\n' >&2
  exit 1
fi
if grep -Fq '<rect' "$generated/native/icon-foreground.svg"; then
  printf 'adaptive foreground contains tile geometry\n' >&2
  exit 1
fi
grep -Fq '<g clip-path="url(#circle)">' "$generated/native/icon-round.svg"
grep -Fq '<background android:drawable="@color/ic_launcher_background" />' \
  "$generated/res/mipmap-anydpi-v26/ic_launcher.xml"
grep -Fq '<foreground android:drawable="@mipmap/ic_launcher_foreground" />' \
  "$generated/res/mipmap-anydpi-v26/ic_launcher.xml"
grep -Fq '<color name="ic_launcher_background">#100c1d</color>' \
  "$generated/res/values/ic_launcher_background.xml"

if [[ $mode == generate ]]; then
  install -m 0644 "$generated/native/icon.svg" "$icons/icon.svg"
  install -m 0644 "$generated/native/icon.png" "$icons/icon.png"
  install -m 0644 "$generated/native/icon-foreground.svg" "$icons/icon-foreground.svg"
  install -m 0644 "$generated/native/icon-round.svg" "$icons/icon-round.svg"
  for root in "$packaged" "$duplicate"; do
    mkdir -p "$root/mipmap-anydpi-v26" "$root/values"
    install -m 0644 "$generated/res/mipmap-anydpi-v26/ic_launcher.xml" \
      "$root/mipmap-anydpi-v26/ic_launcher.xml"
    install -m 0644 "$generated/res/mipmap-anydpi-v26/ic_launcher_round.xml" \
      "$root/mipmap-anydpi-v26/ic_launcher_round.xml"
    install -m 0644 "$generated/res/values/ic_launcher_background.xml" \
      "$root/values/ic_launcher_background.xml"
    for density in "${densities[@]}"; do
      mkdir -p "$root/mipmap-$density"
      install -m 0644 "$generated/res/mipmap-$density"/ic_launcher*.png \
        "$root/mipmap-$density/"
    done
  done
  printf 'Android launcher icons generated\n'
  exit 0
fi

cmp "$generated/native/icon.svg" "$icons/icon.svg"
cmp "$generated/native/icon.png" "$icons/icon.png"
cmp "$generated/native/icon-foreground.svg" "$icons/icon-foreground.svg"
cmp "$generated/native/icon-round.svg" "$icons/icon-round.svg"
for root in "$packaged" "$duplicate"; do
  cmp "$generated/res/mipmap-anydpi-v26/ic_launcher.xml" \
    "$root/mipmap-anydpi-v26/ic_launcher.xml"
  cmp "$generated/res/mipmap-anydpi-v26/ic_launcher_round.xml" \
    "$root/mipmap-anydpi-v26/ic_launcher_round.xml"
  cmp "$generated/res/values/ic_launcher_background.xml" \
    "$root/values/ic_launcher_background.xml"
  for density in "${densities[@]}"; do
    for name in ic_launcher ic_launcher_round ic_launcher_foreground; do
      cmp "$generated/res/mipmap-$density/$name.png" "$root/mipmap-$density/$name.png"
    done
  done
done

printf 'Android launcher icon contract passed\n'
