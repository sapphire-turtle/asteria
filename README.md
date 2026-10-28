# project-asteria

## Running the photos server

```sh
# Create the library (or adopt an existing one) and a config; prints the owner's token once.
asteria-photos-server init --library ~/Pictures --owner me --config ~/.config/asteria/photos.toml
# Add a token for another user listed in <library>/.asteria/users.toml.
asteria-photos-server token add --config ~/.config/asteria/photos.toml --user <USER_ID>
# Serve (Ctrl-C stops it cleanly).
asteria-photos-server serve --config ~/.config/asteria/photos.toml
```

- `init` refuses to overwrite an existing config unless you pass `--force`. Only the token goes to stdout.
- The default address is `127.0.0.1:7810` (`init --listen` sets it; `serve --listen` overrides it).
- The cache (index and thumbnails) lives in `$XDG_CACHE_HOME/asteria-photos/<library-id>/` (usually `~/.cache/...`) and can be deleted safely; ratings and tags live in the library's sidecar files.
- The server's own state (the journal of operations, unfinished uploads, the server lock) lives in `$XDG_DATA_HOME/asteria-photos/<library-id>/` (usually `~/.local/share/...`), not in the library. The library's `.asteria/` holds only the library's own files (`library.toml`, `users.toml`, and later the trash and shared playlists).
- `token add` writes the config only; a running server picks the new token up after a restart. `init --force` writes a new config, which replaces every existing token.
- Tokens travel in clear text (plain HTTP). Expose the server only on a LAN or a VPN such as Tailscale, never directly on the internet.
- One server per library on a host: `serve` holds a lock on `server.lock` in its data directory, and a second one on the same host exits with an error.
- Servers on different hosts may share one library (over NFS, say), such as a production server and a test server. Each has its own journal, cache and unfinished uploads. File watching does not see changes made from another NFS host; they appear after the periodic rescan (every 15 minutes) or `library.rescan`. The duplicate check on upload knows only what this server has scanned. Two servers of the same library on one host are not supported, since they would share the cache and data directories.
- Thumbnails need `vipsthumbnail` (libvips-tools) and, for videos, `ffmpeg`. Set `RUST_LOG` to change log verbosity (default `info`).

## Running the music server

```sh
# Create the library (or adopt an existing one) and a config; prints the owner's token once.
asteria-music-server init --library ~/Music --owner me --config ~/.config/asteria/music.toml
# Add a token for another user listed in <library>/.asteria/users.toml.
asteria-music-server token add --config ~/.config/asteria/music.toml --user <USER_ID>
# Serve (Ctrl-C stops it cleanly).
asteria-music-server serve --config ~/.config/asteria/music.toml
```

- The commands, the config, the token handling, the cache and data directories (`asteria-music` in place of `asteria-photos`), the lock and the plain-HTTP warning are the same as for the photos server above. The default address is `127.0.0.1:7811`.
- It shows the audio files of the library (FLAC, MP3 and the like) along three axes: albums, artists and playlists. Albums and artists are built from the files' tags; a file without tags is still listed, under its file name.
- Playlists are the `.m3u8` and `.m3u` files in the library. An entry whose file has moved is found again by its content hash when the playlist carries one; an entry that cannot be found is reported as missing.
- The tags and the playlists are only read. The server never writes to your music files or playlists.
- The cover of an album is the picture embedded in a track, else an image named `cover`, `folder`, `front` or `album` (`.jpg`, `.jpeg`, `.png` or `.webp`) in the track's folder or, failing that, in the album folder above it (for a disc folder such as `CD1`).
- Covers need `vipsthumbnail` (libvips-tools). The offline copy of a track (Ogg Opus, for listening where the original is too large) needs `ffmpeg` built with libopus. Without them those requests answer 503 and everything else works.

## Running the photos client

```sh
# On Linux, eframe needs a few system packages (Debian/Ubuntu names):
sudo apt install libxkbcommon-dev libwayland-dev libx11-dev libxcb1-dev libgl1-mesa-dev
cargo run -p asteria-photos --release
```

- The client browses a library served by `asteria-photos-server`; it does not need libvips or ffmpeg (the server makes the thumbnails).
- On first start there is no server yet and Settings opens on the Servers page with the add form. Enter a name, the address (such as `http://192.168.1.10:7810`) and the access token that `init` (or `token add`) printed, press "Test connection" to check them, then Save. Open Library on the rail to see the photos.
- You can register several servers. The status bar (bottom right; on a narrow window only its icon) shows which one is in use and whether it is connected; pick another one from its menu to switch, or choose "Manage servers..." (Settings › Servers) to add, edit or delete. The server in use is remembered for the next start, and switching always starts at the library's root.
- If the server cannot be reached, rejects the token or speaks an incompatible version, the library shows the reason with a "Try again" button; there is no offline cache yet.
- Servers and tokens are stored in `servers.toml` in the app's config directory (`~/.config/asteria-photos/` on Linux, `~/Library/Application Support/asteria-photos/` on macOS, `%APPDATA%\asteria-photos\` on Windows). On Unix the file is readable only by you (mode 0600); the tokens are not stored in the OS keychain. Downloaded thumbnails are cached under the app's cache directory (`~/.cache/asteria-photos/` on Linux) and can be deleted safely.
- Select a photo (click) to see its preview and details on the right. In the grid the arrow keys move the selection, and Enter opens the selected photo full screen (as a double-click does): there Left/Right step through the folder and Esc closes the viewer.
- In the details pane you can set the rating (click a star; click the same star again to clear it), the colour label, the keywords and hierarchical tags, and the description (saved when the field loses focus). Edits are written to the library's sidecar files by the server. A photo whose sidecar the server cannot read is shown read-only, with the reason.

## Uploading from the photos client

- Drag files or folders onto the desktop photos window ("Send to Inbox" appears while you hold them over it). Nothing is sent unless a server is connected. Progress is in the Uploads screen and as a count in the status bar.
- Files land under the library's `Inbox/`. A dropped folder keeps its structure (`Inbox/<folder name>/...`); a dropped file goes straight into `Inbox/`. Files in one folder whose names are the same apart from the last extension (`IMG_0001.HEIC` and `IMG_0001.MOV`, or `IMG_0001.HEIC` and its `IMG_0001.HEIC.xmp`) are sent together; `Screenshot ... at 10.15.30.png` and `Screenshot ... at 10.42.07.png` stay separate. If a name is taken in `Inbox/`, the whole group is numbered before the extension (`IMG_0001 (2).HEIC`, `IMG_0001 (2).MOV`).
- Only photos, videos and their XMP sidecars are sent. Anything else, and hidden files, are left out and counted ("N files in the last drop were left out"). A Lightroom-style `IMG_0001.xmp` is sent as the sidecar of the single media file whose name without its extension is `IMG_0001`; if it matches several files it is not sent and a warning is shown. An XMP the server cannot read is dropped and the rest of the group still goes in; the row then says "Done, with warnings" (hover over it for the reason).
- If a file is already in the library, it is not sent: it waits as "Already in the library" until you press "Save" or "Discard" (or "Save all" / "Discard all"), while the other files keep uploading. It is not discarded automatically because a re-release can legitimately share a payload with an existing photo. Failed uploads can be retried with "Try again", and "Cancel" stops one.
- Interrupted uploads resume from where they stopped, including after you close and reopen the app: the queue is kept per library and server profile in the app's data directory. A commit whose answer was lost (a timeout, a dropped connection, closing the app) is asked again rather than sent again, and the server finishes a commit even when the client stopped waiting for it. If a local file changed it is sent again from the start. If the server cannot be reached, uploads wait and try again.
- A server-side session that receives no data for 7 days is deleted, and the client then starts that file over.
- If the server is too old to accept uploads, the Uploads screen says so and nothing is sent.
- Limits: 20 GiB per file; each user can have at most 64 GiB and 1000 unfinished sessions at once (beyond that the server answers with a quota error).
