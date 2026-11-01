#!/bin/sh
# Regenerates the test fixtures with ffmpeg. The committed files are canonical;
# output need not be byte-identical across ffmpeg versions.
set -eu
cd "$(dirname "$0")"
OUT="-map_metadata -1 -fflags +bitexact"
STEREO="sine=frequency=440:duration=2:sample_rate=44100,volume=4,aformat=sample_fmts=s16:channel_layouts=stereo"
ffmpeg -y -loglevel error -f lavfi -i "$STEREO" $OUT tone.flac
ffmpeg -y -loglevel error -f lavfi -i "$STEREO" $OUT -c:a libmp3lame -b:a 64k -write_xing 0 -id3v2_version 0 tone.mp3
ffmpeg -y -loglevel error -f lavfi -i "$STEREO" $OUT -c:a aac -b:a 64k tone.m4a
ffmpeg -y -loglevel error -f lavfi -i "$STEREO" $OUT -c:a alac tone-alac.m4a
ffmpeg -y -loglevel error -f lavfi -i "$STEREO" $OUT -c:a libvorbis -b:a 64k tone.ogg
ffmpeg -y -loglevel error -f lavfi -i "$STEREO" $OUT -c:a libopus -b:a 96k -ar 48000 tone.opus
ffmpeg -y -loglevel error -f lavfi -i "sine=frequency=440:duration=1:sample_rate=22050,volume=4,aformat=sample_fmts=s16:channel_layouts=mono" $OUT tone-mono-22k.flac
head -c 200 tone.flac > broken.flac
