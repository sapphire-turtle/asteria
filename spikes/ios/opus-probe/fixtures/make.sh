#!/bin/sh
# Regenerates sample.opus: 10 s of a 440 Hz sine mixed with pink noise, stereo, 96 kbit/s libopus.
set -eu
cd "$(dirname "$0")"
ffmpeg -f lavfi -i "sine=frequency=440:duration=10" -f lavfi -i "anoisesrc=color=pink:amplitude=0.1:duration=10" -filter_complex "[0][1]amix=inputs=2,aformat=channel_layouts=stereo" -c:a libopus -b:a 96k -y sample.opus
