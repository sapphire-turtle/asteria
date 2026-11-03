<img src="docs/icons/asteria-photos.svg" alt="asteria photos" width="64" height="64"> <img src="docs/icons/asteria-music.svg" alt="asteria music" width="64" height="64">

# project-asteria

## Installing

Every release on [the releases page](https://github.com/sapphire-turtle/asteria/releases) carries the two desktop apps as installable packages and the two servers without a window as single files. Pre-releases (such as `v0.1.0-rc.1`) are marked as such.

The desktop app is the window, the server's command line (`asteria-photos` / `asteria-music`) and, if you want one, a server on this computer, in one install. Download the file for your system (`photos` or `music` in the name):

- Windows: `asteria-photos-desktop_<version>_x64-setup.exe` installs it for you alone (in `%LocalAppData%\Asteria Photos`, with a Start menu entry) and puts `asteria-photos` on `Path`; uninstall it from Settings › Apps.
- macOS: `asteria-photos-desktop_<version>_universal.dmg`, one app for Apple silicon and Intel Macs; drag it to Applications. Settings › About offers to put `asteria-photos` on `PATH`.
- Linux: `asteria-photos-desktop_<version>_x86_64.AppImage`; make it executable (`chmod +x`) and run it. Settings › About offers to put `asteria-photos` on `PATH`.

An installed app looks for a new release once a day, downloads it in the background, checks its signature, and applies it when you choose to (Settings › About), or at its next start. (The `.sig` files and `asteria-<app>-latest.json` on a release are what it reads; the `.app.tar.gz` is the macOS app for its updates.)

- The Windows and macOS packages include the external tools the server uses (for photos only: ffmpeg, and on Windows libvips's `vipsthumbnail`; music needs none), with their licences in `tools/LICENSES` (ffmpeg is GPLv3, libvips LGPL). On Linux install them with your package manager (see [A server on this computer](#a-server-on-this-computer)).
- The Windows installer is not signed yet: SmartScreen warns before it runs ("More info" › "Run anyway"). The macOS app is signed and notarized when the release workflow has a Developer ID; until then Gatekeeper refuses to open it at first (allow it with "Open Anyway" in System Settings › Privacy & Security).

The servers without a window are `asteria-photos-headless-<platform>` and `asteria-music-headless-<platform>`, for `linux-x86_64`, `linux-aarch64`, `macos-aarch64` and `windows-x86_64.exe`. Rename the file to `asteria-photos` (or `asteria-music`; keep `.exe` on Windows), make it executable and put it on `PATH`. `asteria-photos update` replaces it with the newest release (`--check` only tells you whether there is one, `-y` does not ask); restart a running `serve` afterwards. A pre-release (rc) binary is not offered later pre-releases by `update`: download those by hand. A server installed with `cargo install` is updated with `cargo install` instead, and the desktop app updates itself.

## Running the photos server

`asteria-photos` is the server without a window, for hosts with no GUI. Install it with `cargo install --path photos/headless` (building the servers needs `cmake`, for the bundled libopus); the desktop app (`asteria-photos-desktop`) runs the same commands. On Windows a `cargo install`ed desktop app is a GUI program: for the command line use the packaged app's `asteria-photos` (its console shim) or the server without a window.

```sh
# Create the library (or adopt an existing one) and a config; prints the token of the key `owner` once.
asteria-photos init --library ~/Pictures --owner me
# Add a key (and print its token once) for another device.
asteria-photos api-key add laptop --profile me
# Serve (Ctrl-C stops it cleanly).
asteria-photos serve
```

- The config is `<config dir>/asteria-photos/server.toml` unless `--config <file>` names another place (every command takes it). `api-key list|retire|restore|rotate` manage the keys. A key's profile is a user name, and the key acts as that user. For now the only user is the owner, whose name `--owner` sets: 1 to 32 ASCII letters, digits, `-` or `_`.
- `init` refuses to overwrite an existing config unless you pass `--force`. Only the token goes to stdout.
- The default address is `127.0.0.1:7810` (`init --listen` sets it; `serve --listen` overrides it).
- The cache (index and thumbnails) lives in `$XDG_CACHE_HOME/asteria-photos/<library-id>/` (usually `~/.cache/...`) and can be deleted safely; ratings and tags live in the library's sidecar files.
- The server's own state (the journal of operations, unfinished uploads, the server lock) lives in `$XDG_DATA_HOME/asteria-photos/<library-id>/` (usually `~/.local/share/...`), not in the library. The library's `.asteria/` holds only the library's own files (`library.toml`, `users.toml`, and later the trash and shared playlists).
- Keys are files in the data directory, so a key added with `api-key add` works without restarting a running server. `init --force` writes a new config and retires every existing key.
- Tokens travel in clear text (plain HTTP). Expose the server only on a LAN or a VPN such as Tailscale, never directly on the internet.
- One server per library on a host: `serve` holds a lock on `server.lock` in its data directory, and a second one on the same host exits with an error.
- Upgrading from a version with `[[token]]` entries in the config: the first command that opens it imports them as API keys `token-1`, `token-2`… and rewrites the config without them (comments in the file are lost; if the file is read-only, the server warns and you remove the `[[token]]` entries by hand). The tokens keep working, but a token that was issued for another user now acts as the owner too. Keys added before profiles existed (asteria with corundum 0.6) are given the owner's user name the first time a command opens the keys.
- Servers on different hosts may share one library (over NFS, say), such as a production server and a test server. Each has its own journal, cache and unfinished uploads. File watching does not see changes made from another NFS host; they appear after the periodic rescan (every 15 minutes) or `library.rescan`. The duplicate check on upload knows only what this server has scanned. Two servers of the same library on one host are not supported, since they would share the cache and data directories.
- Thumbnails of common formats (JPEG, PNG, WebP, GIF, TIFF, RAW previews) are made by the server itself. `vipsthumbnail` (libvips-tools) is needed only for HEIC, AVIF, some camera RAW files and very large images, and `ffmpeg` for videos, found on `PATH` or named in `ASTERIA_VIPSTHUMBNAIL` / `ASTERIA_FFMPEG`. Set `RUST_LOG` to change log verbosity (default `info`).

## Running the music server

`asteria-music` is the server without a window (`cargo install --path music/headless`; building it needs `cmake`); the desktop app (`asteria-music-desktop`) runs the same commands. On Windows a `cargo install`ed desktop app is a GUI program: for the command line use the packaged app's `asteria-music` (its console shim) or the server without a window.

```sh
# Create the library (or adopt an existing one) and a config; prints the token of the key `owner` once.
asteria-music init --library ~/Music --owner me
# Add a key (and print its token once) for another device.
asteria-music api-key add laptop --profile me
# Serve (Ctrl-C stops it cleanly).
asteria-music serve
```

- The commands, the config, the token handling, the cache and data directories (`asteria-music` in place of `asteria-photos`), the lock and the plain-HTTP warning are the same as for the photos server above. The default address is `127.0.0.1:7811`.
- It shows the audio files of the library (FLAC, MP3 and the like) along three axes: albums, artists and playlists. Albums and artists are built from the files' tags; a file without tags is still listed, under its file name.
- Playlists are the `.m3u8` and `.m3u` files in the library. An entry whose file has moved is found again by its content hash when the playlist carries one; an entry that cannot be found is reported as missing.
- The tags and the playlists are only read. The server never writes to your music files or playlists.
- The cover of an album is the picture embedded in a track, else an image named `cover`, `folder`, `front` or `album` (`.jpg`, `.jpeg`, `.png` or `.webp`) in the track's folder or, failing that, in the album folder above it (for a disc folder such as `CD1`).
- Covers are made by the server itself, with no external tool. The server also makes the offline copy of a track (Ogg Opus, for listening where the original is too large) itself, with no external tool.

## Connecting an MCP client

Both servers speak the Model Context Protocol, so an AI agent can browse the library and tidy it up.

```sh
# Claude Code; <URL> is the server's address, such as http://127.0.0.1:7811
claude mcp add --transport http asteria <URL>/mcp --header "Authorization: Bearer <token>"
```

- The endpoint is `<server URL>/mcp` (Streamable HTTP). The token is the same Bearer token the clients use; the agent acts as the library's owner.
- The tools are the library, folder and item calls, `media_update` for ratings, tags and descriptions, `organize_apply` and its status, undo and history, and `media_preview` for thumbnails. The music server adds `album_list`, `album_get`, `artist_list`, `artist_get`, `playlist_list` and `playlist_get`.
- What the agent changes is recorded in the journal as `agent`, and `organize_undo` takes a batch of file operations back. Metadata edits (`media_update`) are not batches and cannot be undone.

## Running the photos client

```sh
# On Linux, eframe needs a few system packages (Debian/Ubuntu names):
sudo apt install libxkbcommon-dev libwayland-dev libx11-dev libxcb1-dev libgl1-mesa-dev
cargo run -p asteria-photos-desktop --release
# The same binary with arguments is the server's command line: `asteria-photos serve`, say.
cargo run -p asteria-photos-desktop --release -- serve
```

- The client browses a library served by `asteria-photos serve`; it does not need libvips or ffmpeg (the server makes the thumbnails).
- On first start there is no server yet, and the window asks where your library is: on this computer (see [A server on this computer](#a-server-on-this-computer)), on another server, or later. For another server (or later), Settings opens on the Servers page with the add form. Enter a name, the address (such as `http://192.168.1.10:7810`) and the access token that `init` (or `api-key add`) printed, press "Test connection" to check them, then Save. Open Library on the rail to see the photos.
- You can register several servers. The status bar (bottom right; on a narrow window only its icon) shows which one is in use and whether it is connected; pick another one from its menu to switch, or choose "Manage servers..." (Settings › Servers) to add, edit or delete. The server in use is remembered for the next start, and switching always starts at the library's root.
- If the server cannot be reached, rejects the token or speaks an incompatible version, the library shows the reason with a "Try again" button; there is no offline cache yet.
- Servers and tokens are stored in `servers.toml` in the app's config directory (`~/.config/asteria-photos/` on Linux, `~/Library/Application Support/asteria-photos/` on macOS, `%LOCALAPPDATA%\asteria-photos\` on Windows; an earlier version's settings under `%APPDATA%` move there on the first start). On Unix the file is readable only by you (mode 0600); the tokens are not stored in the OS keychain. Downloaded thumbnails are cached under the app's cache directory (`~/.cache/asteria-photos/` on Linux) and can be deleted safely.
- Select a photo (click) to see its preview and details on the right. In the grid the arrow keys move the selection, and Enter opens the selected photo full screen (as a double-click does): there Left/Right step through the folder and Esc closes the viewer.
- In the details pane you can set the rating (click a star; click the same star again to clear it), the colour label, the keywords and hierarchical tags, and the description (saved when the field loses focus). Edits are written to the library's sidecar files by the server. A photo whose sidecar the server cannot read is shown read-only, with the reason.

## Running the music client

```sh
# On Linux, eframe needs a few system packages, and the sound output needs ALSA (Debian/Ubuntu names):
sudo apt install libxkbcommon-dev libwayland-dev libx11-dev libxcb1-dev libgl1-mesa-dev libasound2-dev
cargo run -p asteria-music-desktop --release
# The same binary with arguments is the server's command line: `asteria-music serve`, say.
cargo run -p asteria-music-desktop --release -- serve
```

- The client plays a library served by `asteria-music serve`; it does not need libvips or ffmpeg (the server makes the covers and the low-quality copies).
- On first start the window asks where your library is, as in the photos client. For another server (or later), Settings opens on the Servers page with the add form. Enter a name, the address (such as `http://192.168.1.10:7811`) and the access token that `init` (or `api-key add`) printed, press "Test connection" to check them, then Save. Servers are registered, switched and stored as in the photos client (in `servers.toml`, under `asteria-music` in place of `asteria-photos`).
- Pick an album, an artist or a playlist to see its tracks and play them. Playback starts before the file has been fully downloaded, and seeking to a position that has not arrived yet waits for that part.
- "Play in low quality" is one setting for the whole app, saved in `playback.toml` in the app's config directory along with the volume. When it is on, the server converts each track to low-bitrate Ogg Opus (the offline copy) and the client plays that; it is meant for slow connections. The first play of a track can show "Preparing" while the server converts it. A change takes effect from the next track you open.
- Not there yet: showing the library by folder (#20), ratings (#19), shuffle and repeat (#17), gapless playback (#18), and the media keys (`sapphire-turtle/corundum-framework#50`).

## A server on this computer

The desktop apps can set up a server for a folder on this computer, so one machine needs nothing else. The window still only connects to the server; the server is `asteria-photos serve` (or `asteria-music serve`) running in the background.

- On first start, when no server is registered, choose "This computer", enter your user name (the OS user's is suggested; it becomes the owner of a library created here), and pick the library's folder (a folder that is already a library is used as it is, with its own owner). The app writes the server's config to its default place, creates an API key named `desktop` that acts as the owner, and registers the server as "This computer" (`http://127.0.0.1:7810`, or `:7811` for music) and uses it. It then offers to keep the server running in the background: a systemd user unit on Linux, a LaunchAgent on macOS, a logon task in the Task Scheduler on Windows. If you decline, start it yourself with `asteria-photos serve`. Last, it shows which external tools it found.
- Settings › Local server shows the library and the address the server listens on, registers, starts, stops or removes the background service, manages the library's API keys (add one for each phone or other computer, retire one you no longer use), and shows the external tools. Without a local server yet, its "Set up a local server" button runs the first-start steps again.
- The server makes thumbnails with external tools; without them it still runs, just without thumbnails. Install them where the server runs:
  - `vipsthumbnail` (from libvips), for the HEIC, AVIF and some camera RAW photos the server cannot decode itself (macOS's `sips` stands in for most of them): `sudo apt install libvips-tools` (Debian/Ubuntu; other distributions name the package similarly), `brew install vips` on macOS, or on Windows the libvips build from github.com/libvips/build-win64-mxe/releases with its `bin` folder added to `PATH`.
  - `ffmpeg`, for photos' video thumbnails (music does not use it; it makes its offline copies itself): `sudo apt install ffmpeg`, `brew install ffmpeg`, or on Windows a build from ffmpeg.org/download.html with its `bin` folder added to `PATH`.
  - The Windows and macOS packages of the desktop app already include what the app uses (photos: ffmpeg on macOS, where the system's `sips` stands in for `vipsthumbnail`, and both on Windows; music: nothing).
  - Or point `ASTERIA_VIPSTHUMBNAIL` / `ASTERIA_FFMPEG` at the programs. Otherwise they are looked for beside the executable (where the packages put them), then on `PATH`, then in `/opt/homebrew/bin` and `/usr/local/bin` (macOS) or `/usr/local/bin` (Linux). "Check again" on the page looks for them again.

## Uploading from the photos client

- Drag files or folders onto the desktop photos window ("Send to Inbox" appears while you hold them over it). Nothing is sent unless a server is connected. Progress is in the Uploads screen and as a count in the status bar.
- Files land under the library's `Inbox/`. A dropped folder keeps its structure (`Inbox/<folder name>/...`); a dropped file goes straight into `Inbox/`. Files in one folder whose names are the same apart from the last extension (`IMG_0001.HEIC` and `IMG_0001.MOV`, or `IMG_0001.HEIC` and its `IMG_0001.HEIC.xmp`) are sent together; `Screenshot ... at 10.15.30.png` and `Screenshot ... at 10.42.07.png` stay separate. If a name is taken in `Inbox/`, the whole group is numbered before the extension (`IMG_0001 (2).HEIC`, `IMG_0001 (2).MOV`).
- Only photos, videos and their XMP sidecars are sent. Anything else, and hidden files, are left out and counted ("N files in the last drop were left out"). A Lightroom-style `IMG_0001.xmp` is sent as the sidecar of the single media file whose name without its extension is `IMG_0001`; if it matches several files it is not sent and a warning is shown. An XMP the server cannot read is dropped and the rest of the group still goes in; the row then says "Done, with warnings" (hover over it for the reason).
- If a file is already in the library, it is not sent: it waits as "Already in the library" until you press "Save" or "Discard" (or "Save all" / "Discard all"), while the other files keep uploading. It is not discarded automatically because a re-release can legitimately share a payload with an existing photo. Failed uploads can be retried with "Try again", and "Cancel" stops one.
- Interrupted uploads resume from where they stopped, including after you close and reopen the app: the queue is kept per library and server profile in the app's data directory. A commit whose answer was lost (a timeout, a dropped connection, closing the app) is asked again rather than sent again, and the server finishes a commit even when the client stopped waiting for it. If a local file changed it is sent again from the start. If the server cannot be reached, uploads wait and try again.
- A server-side session that receives no data for 7 days is deleted, and the client then starts that file over.
- If the server is too old to accept uploads, the Uploads screen says so and nothing is sent.
- Limits: 20 GiB per file; each user can have at most 64 GiB and 1000 unfinished sessions at once (beyond that the server answers with a quota error).
