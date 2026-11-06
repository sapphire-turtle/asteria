# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/sapphire-turtle/asteria/releases/tag/v0.1.0) - 2026-11-06

The first release: two self-hosted apps, photos and music, each a server for a library of
your own files and a desktop app that browses it, and an MCP endpoint for AI agents. Earlier
builds were the pre-releases `v0.1.0-rc.1` to `rc.3`.

### Libraries and servers

- A library is a folder of your files, used as it is: metadata goes in standard places (XMP
  sidecars for photos; tags and `.m3u8` / `.m3u` playlists for music), and the server's
  index, thumbnails and journal live outside it and can be rebuilt.
- `asteria-photos` and `asteria-music`: `init` (create or adopt a library), `serve`,
  `api-key add|list|retire|restore|rotate` (one key per device, acting as the library's
  owner), and `update`, which replaces a downloaded binary with the newest release.
- The library is watched for changes and rescanned every 15 minutes; files are identified
  by content hash, so a moved file keeps its ratings and playlist entries.
- Thumbnails and covers are made by the server itself for the common formats. Photos uses
  `vipsthumbnail` for HEIC, AVIF, some RAW and very large images, and `ffmpeg` for video;
  music needs no external tool, and makes its low-bitrate Ogg Opus copies itself.
- Uploads into `Inbox/` (photos): resumable chunked sessions, a duplicate check by content
  hash, and grouped files (a Live Photo's HEIC and MOV) placed together or not at all.
- File operations (move, rename, trash) in batches with a dry run, a journal, history and
  undo. A move never replaces an existing file (one atomic rename where the OS has one),
  never goes through a symbolic link out of the library, and renames that change only the
  case work on case-insensitive filesystems.
- Several servers on different hosts may share one library (over NFS, say), each with its
  own journal and cache.

### MCP

- Both servers speak the Model Context Protocol at `<server>/mcp`: browsing the library,
  `media_update` for ratings, tags and descriptions, `organize_apply` with its status, undo
  and history, and `media_preview`; music adds albums, artists and playlists. What an agent
  changes is journaled as `agent`.

### Desktop apps

- Photos: a grid of the library's folders with thumbnails, a detail pane with a preview,
  the rating, colour label, keywords, hierarchical tags and description (written to the
  sidecars by the server), a full-screen viewer, and uploads by drag and drop.
- Music: albums, artists and playlists, streamed playback that starts before the download
  ends and seeks within it, the volume, and a low-quality mode for slow connections.
- Several servers can be registered and switched from the status bar. On first start the
  app asks where the library is: on another server, or on this computer, where it sets up
  the server, its API key and (if you want) a background service (systemd, launchd, Task
  Scheduler), managed later in Settings › Local server.
- Settings › About shows the version and updates: an installed app checks for a new
  release once a day, verifies its signature and applies it when you choose to.
- English and Japanese.

### Packages

- On each release: the desktop apps as a Windows installer, a universal macOS DMG and a
  Linux AppImage, signed for the updater (the Windows installer is not code-signed yet),
  and the headless servers for Linux x86_64 and aarch64, macOS aarch64 and Windows x86_64.
  The Windows and macOS photos packages carry ffmpeg (and on Windows libvips), outside the
  user's `PATH`.

### Known limitations

- No offline cache in the clients yet: without a connection to the server, the library is
  not shown.
- One user per library (the owner); tokens travel over plain HTTP, so expose a server only
  on a LAN or a VPN.
- Music does not yet show the library by folder, ratings, shuffle and repeat, gapless
  playback or the media keys.
