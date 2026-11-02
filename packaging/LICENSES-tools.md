# Third-party tools in this package

The asteria desktop packages for Windows and macOS carry these programs unmodified, so that a
server on this computer can make thumbnails without installing anything: Asteria Photos
carries FFmpeg (Windows and macOS) and libvips (Windows); Asteria Music carries libvips
(Windows) only, as it makes its offline copies of music itself. Only the programs this
package carries, and their licence texts, are in it.
asteria itself is under the GPL-3.0-or-later (with MIT and Apache-2.0 for some parts; see the
project's repository, https://github.com/sapphire-turtle/asteria). The licence texts are in
this directory.

## FFmpeg (Asteria Photos, Windows and macOS)

- `ffmpeg.exe` (Windows) and `ffmpeg` (macOS), FFmpeg 9.0.2.
- Licence: GNU General Public License, version 3 (both builds are configured with
  `--enable-gpl --enable-version3`). Text: `ffmpeg-LICENSE.txt` (Windows),
  `ffmpeg-LICENSE-GPLv3.txt` (macOS).
- Source of FFmpeg: https://ffmpeg.org/download.html (release 9.0.2) and
  https://git.ffmpeg.org/ffmpeg.git.
- Windows build: BtbN/FFmpeg-Builds, release `autobuild-2026-11-01-12-49`, file
  `ffmpeg-n9.0.2-42-gd7643a0fe3-win64-gpl-9.0.zip`. The build scripts and the versions of
  every library linked in: https://github.com/BtbN/FFmpeg-Builds (that tag).
- macOS build: https://ffmpeg.martin-riedl.de, builds `1789931890_9.0.2` (arm64) and
  `1789931006_9.0.2` (x86_64), joined into one universal file. The build scripts:
  https://git.martin-riedl.de/ffmpeg/build-script; the configuration and the library
  versions are listed beside each download on that site.

## libvips (Windows, both apps)

- `vips/bin/` (`vipsthumbnail.exe`, libvips and the libraries it loads), libvips 8.18.7.
- Licence: GNU Lesser General Public License, version 2.1 or later, for libvips. Text:
  `libvips-LICENSE.txt`. The other libraries in `vips/bin/` are under their own free
  licences (LGPL, GPL for poppler, MIT, BSD and the like), all compatible with distributing
  them alongside GPLv3 software.
- Source: https://github.com/libvips/libvips (tag v8.18.7). The Windows build:
  https://github.com/libvips/build-win64-mxe, release v8.18.7, file
  `vips-dev-x64-all-8.18.7.zip`; `libvips-versions.json` names the version of each bundled
  library, whose sources that repository's build fetches.
