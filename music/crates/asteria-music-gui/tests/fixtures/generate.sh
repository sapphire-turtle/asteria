#!/bin/sh
# Regenerates the test fixture with ffmpeg. The committed file is canonical;
# output need not be byte-identical across ffmpeg versions.
set -eu
cd "$(dirname "$0")"
OUT="-map_metadata -1 -fflags +bitexact"
STEREO="sine=frequency=440:duration=2:sample_rate=44100,volume=4,aformat=sample_fmts=s16:channel_layouts=stereo"
ffmpeg -y -loglevel error -f lavfi -i "$STEREO" $OUT tone.flac
# The offline-audio rendition the server makes (Ogg Opus), as saved for offline.
ffmpeg -y -loglevel error -i tone.flac -c:a libopus -b:a 96k $OUT tone.opus
