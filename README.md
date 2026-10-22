# project-asteria

## Running the gallery server

```sh
# Create the library (or adopt an existing one) and a config; prints the owner's token once.
asteria-gallery-server init --library ~/Pictures --owner me --config ~/.config/asteria/gallery.toml
# Add a token for another user listed in <library>/.asteria/users.toml.
asteria-gallery-server token add --config ~/.config/asteria/gallery.toml --user <USER_ID>
# Serve (Ctrl-C stops it cleanly).
asteria-gallery-server serve --config ~/.config/asteria/gallery.toml
```

- `init` refuses to overwrite an existing config unless you pass `--force`. Only the token goes to stdout.
- The default address is `127.0.0.1:7810` (`init --listen` sets it; `serve --listen` overrides it).
- The cache (index and thumbnails) lives in `$XDG_CACHE_HOME/asteria-gallery/<library-id>/` (usually `~/.cache/...`) and can be deleted safely; ratings and tags live in the library's sidecar files.
- `token add` writes the config only; a running server picks the new token up after a restart. `init --force` writes a new config, which replaces every existing token.
- Tokens travel in clear text (plain HTTP). Expose the server only on a LAN or a VPN such as Tailscale, never directly on the internet.
- One server per library: `serve` holds a lock on `<library>/.asteria/server.lock` and a second one exits with an error.
- Thumbnails need `vipsthumbnail` (libvips-tools) and, for videos, `ffmpeg`. Set `RUST_LOG` to change log verbosity (default `info`).
