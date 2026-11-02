#!/usr/bin/env bash
# Puts the external tools a desktop package carries (spec §5.9) into <dest>, the directory
# that becomes the package (`vpk pack --packDir`), beside the main executable, where the
# server looks first:
#
#   photos on windows: <dest>/ffmpeg.exe and <dest>/vips/bin/vipsthumbnail.exe with
#                      libvips's DLLs
#   photos on macos:   <dest>/ffmpeg, one universal binary of the arm64 and x86_64 builds
#   music on windows:  <dest>/vips/bin/vipsthumbnail.exe with libvips's DLLs (cover
#                      thumbnails; the server makes offline audio itself)
#   music on macos:    nothing (macOS's sips makes the cover thumbnails)
#
# and their licences into <dest>/tools/LICENSES. Each download is checked against the
# SHA-256 in packaging/tools.env, and ffmpeg must list the libwebp encoder, or the script
# fails and the release stops. The programs are only run on the OS they are for; elsewhere
# (a Linux machine trying the script) the checks are skipped with a note.
#
# usage: packaging/fetch-tools.sh <windows|macos> <photos|music> <dest>
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=packaging/tools.env
. "$here/tools.env"

if [ $# -ne 3 ] || { [ "$1" != windows ] && [ "$1" != macos ]; } ||
  { [ "$2" != photos ] && [ "$2" != music ]; }; then
  echo "usage: $0 <windows|macos> <photos|music> <dest>" >&2
  exit 2
fi
os=$1
app=$2
dest=$3

# What the package carries: ffmpeg for photos' video thumbnails, and on Windows libvips for
# both apps' image thumbnails.
ffmpeg=no
vips=no
[ "$app" = photos ] && ffmpeg=yes
[ "$os" = windows ] && vips=yes
if [ $ffmpeg = no ] && [ $vips = no ]; then
  echo "The $app package for $os carries no tools."
  exit 0
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
licenses="$dest/tools/LICENSES"
mkdir -p "$licenses"
cp "$here/LICENSES-tools.md" "$licenses/README.md"

sha256() {
  if command -v sha256sum > /dev/null; then
    sha256sum "$1" | cut -d ' ' -f 1
  else
    shasum -a 256 "$1" | cut -d ' ' -f 1
  fi
}

# fetch <file> <url> <sha256>: downloads <url> to $work/<file> and fails unless its SHA-256
# is the pinned one.
fetch() {
  echo "Downloading $2"
  curl -fsSL --retry 3 -o "$work/$1" "$2"
  local actual
  actual=$(sha256 "$work/$1")
  if [ "$actual" != "$3" ]; then
    echo "error: $2 has SHA-256 $actual, not the pinned $3" >&2
    exit 1
  fi
  echo "$1: SHA-256 matches"
}

# check_encoders <command...>: fails unless the ffmpeg the command runs lists the encoder
# the server needs: libwebp (video thumbnails).
check_encoders() {
  local encoders
  encoders=$("$@" -hide_banner -encoders)
  if ! grep -qw libwebp <<< "$encoders"; then
    echo "error: $* has no libwebp encoder" >&2
    exit 1
  fi
  echo "$*: libwebp present"
}

case $os in
  windows)
    if [ $ffmpeg = yes ]; then
      fetch ffmpeg.zip "$WIN_FFMPEG_URL" "$WIN_FFMPEG_SHA256"
      unzip -q "$work/ffmpeg.zip" -d "$work/ffmpeg"
      # The archive holds one directory named after its version.
      ffmpeg_dir=$(echo "$work"/ffmpeg/*/)
      cp "$ffmpeg_dir/bin/ffmpeg.exe" "$dest/ffmpeg.exe"
      cp "$ffmpeg_dir/LICENSE.txt" "$licenses/ffmpeg-LICENSE.txt"
    fi

    fetch vips.zip "$WIN_VIPS_URL" "$WIN_VIPS_SHA256"
    unzip -q "$work/vips.zip" -d "$work/vips"
    vips_dir=$(echo "$work"/vips/*/)
    # bin: the programs, the DLLs they load and libvips's modules; etc: the configuration
    # files libvips's dependencies look for beside bin.
    mkdir -p "$dest/vips"
    cp -R "$vips_dir/bin" "$vips_dir/etc" "$dest/vips/"
    cp "$vips_dir/LICENSE" "$licenses/libvips-LICENSE.txt"
    # The versions of the libraries bundled with libvips, to find their sources by.
    cp "$vips_dir/versions.json" "$licenses/libvips-versions.json"

    case $(uname -s) in
      MINGW* | MSYS* | CYGWIN*)
        if [ $ffmpeg = yes ]; then
          check_encoders "$dest/ffmpeg.exe"
        fi
        "$dest/vips/bin/vipsthumbnail.exe" --vips-version
        ;;
      *)
        echo "note: not on Windows ($(uname -s)): ffmpeg.exe's encoders and vipsthumbnail.exe are not checked"
        ;;
    esac
    ;;

  macos)
    fetch ffmpeg-arm64.zip "$MAC_FFMPEG_ARM64_URL" "$MAC_FFMPEG_ARM64_SHA256"
    fetch ffmpeg-x86_64.zip "$MAC_FFMPEG_X86_64_URL" "$MAC_FFMPEG_X86_64_SHA256"
    for arch in arm64 x86_64; do
      unzip -q "$work/ffmpeg-$arch.zip" -d "$work/$arch"
      chmod +x "$work/$arch/ffmpeg"
    done
    # The builds are configured with --enable-gpl --enable-version3 and carry no licence
    # file of their own; the GPLv3's text is the one asteria is under.
    cp "$here/../LICENSE-GPL" "$licenses/ffmpeg-LICENSE-GPLv3.txt"

    if [ "$(uname -s)" = Darwin ]; then
      # Each half before joining them, so that a failure names the build; on Apple silicon
      # the x86_64 half runs under Rosetta.
      if [ "$(uname -m)" = arm64 ]; then
        check_encoders "$work/arm64/ffmpeg"
      else
        echo "note: not on Apple silicon: the arm64 half is checked only once joined"
      fi
      check_encoders arch -x86_64 "$work/x86_64/ffmpeg"
      lipo -create -output "$dest/ffmpeg" "$work/arm64/ffmpeg" "$work/x86_64/ffmpeg"
      lipo -info "$dest/ffmpeg"
      check_encoders "$dest/ffmpeg"
    else
      echo "note: not on macOS ($(uname -s)): the ffmpeg builds are not checked or joined, and $dest/ffmpeg is not written"
      ls -l "$work/arm64/ffmpeg" "$work/x86_64/ffmpeg"
    fi
    ;;
esac

echo "Tools in $dest:"
find "$dest" -maxdepth 1
