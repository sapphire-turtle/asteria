#!/bin/sh
# Regenerates the test fixtures with ffmpeg. The committed files are canonical;
# output need not be byte-identical across ffmpeg versions.
set -eu
cd "$(dirname "$0")"
OUT="-map_metadata -1 -fflags +bitexact"
for c in red blue; do
  for ext in jpg png; do
    ffmpeg -y -loglevel error -f lavfi -i "color=$c:s=16x16" -frames:v 1 $OUT "$c.$ext"
  done
  ffmpeg -y -loglevel error -f lavfi -i "color=$c:s=16x16" -frames:v 1 -lossless 1 $OUT "$c.webp"
done
for f in 440 880; do
  ffmpeg -y -loglevel error -f lavfi -i "sine=frequency=$f:duration=0.2" $OUT "sine$f.flac"
  ffmpeg -y -loglevel error -f lavfi -i "sine=frequency=$f:duration=0.2" $OUT -write_xing 0 -id3v2_version 0 "sine$f.mp3"
done
