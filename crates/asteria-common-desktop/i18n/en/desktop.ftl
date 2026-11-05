# asteria-common-desktop's strings. Keys are `asteria-common-desktop-<area>-<what>`.

# The server on this computer, as the servers list names it.
asteria-common-desktop-this-computer = This computer

# First run: where the library is.
asteria-common-desktop-where-title = Where is your library?
asteria-common-desktop-where-this-computer-note = Choose a folder on this computer. A server runs here and the app connects to it.
asteria-common-desktop-where-other = Another server
asteria-common-desktop-where-other-note = The library is served by another computer. Add that server in Servers.
asteria-common-desktop-where-later = Later
asteria-common-desktop-where-later-note = Set it up later, in Settings.

# First run: setting up the server on this computer.
asteria-common-desktop-setup-title = Set up a library on this computer
asteria-common-desktop-setup-intro = Choose the library's folder. A folder that is already a library is used as it is; any other becomes one.
asteria-common-desktop-setup-owner = Your user name
asteria-common-desktop-setup-owner-note = The library's owner, for a library made here. What you do in the app is recorded as this user.
asteria-common-desktop-setup-owner-invalid = Use 1 to { $max } ASCII letters, digits, - or _.
asteria-common-desktop-setup-existing-owner = The folder was already a library, so its owner { $owner } is kept.
asteria-common-desktop-setup-choose = Choose a folder…
asteria-common-desktop-setup-picker-title = The library's folder
asteria-common-desktop-setup-running = Setting up { $path }…
asteria-common-desktop-setup-done = The library at { $path } is ready, owned by { $owner }. The app connects to it at { $url }.
asteria-common-desktop-setup-failed = The library could not be set up: { $error }
asteria-common-desktop-setup-stopped = the setup stopped unexpectedly
asteria-common-desktop-setup-try-again = Try again
asteria-common-desktop-setup-other-folder = Choose another folder

# First run: the background service.
asteria-common-desktop-service-question = Keep the server running in the background?
asteria-common-desktop-service-reason = The app shows the library through the server. In the background it starts when you log in and keeps watching the library.
asteria-common-desktop-not-running = The server is not running. To start it, run:

# The external tools.
asteria-common-desktop-tools-title = External tools
asteria-common-desktop-tools-intro = The server makes most thumbnails itself. These programs add what it cannot read (HEIC, AVIF, some camera RAW, very large images) and video. Without them it still runs.
asteria-common-desktop-tools-vips-purpose = Thumbnails of HEIC, AVIF and some camera RAW files.
asteria-common-desktop-tools-ffmpeg-purpose = Video thumbnails.
asteria-common-desktop-tools-found = Found: { $path }
asteria-common-desktop-tools-missing = Not found.
asteria-common-desktop-tools-sips = Not installed: those images are made with sips, which macOS has built in.
asteria-common-desktop-tools-linux = Install the package { $package } with your distribution's package manager.
asteria-common-desktop-tools-macos = Install it with Homebrew: brew install { $package }
asteria-common-desktop-tools-windows-vips = Download libvips for Windows (github.com/libvips/build-win64-mxe/releases), and add its bin folder to PATH.
asteria-common-desktop-tools-windows-ffmpeg = Download FFmpeg for Windows (ffmpeg.org/download.html), and add its bin folder to PATH.
asteria-common-desktop-tools-env = Or set { $var } to the program's path.
asteria-common-desktop-tools-none = The server of this app needs no external tools.
asteria-common-desktop-tools-check-again = Check again

# The "Local server" settings page.
asteria-common-desktop-page-label = Local server
asteria-common-desktop-page-not-set-up = No local server is set up yet.
asteria-common-desktop-page-set-up = Set up a local server
asteria-common-desktop-page-unreadable = The local server's config could not be read: { $error }
asteria-common-desktop-page-library = Library
asteria-common-desktop-page-listen = Listening on
asteria-common-desktop-page-config = Config file
asteria-common-desktop-page-listen-note = To accept connections from other devices, change listen in the config file.
asteria-common-desktop-page-service = Background server
asteria-common-desktop-page-service-note = Without the background service, start the server yourself:
asteria-common-desktop-page-keys = API keys
asteria-common-desktop-page-keys-failed = The API keys could not be opened: { $error }

# Settings › About.
asteria-common-desktop-about = About
asteria-common-desktop-about-version = Version { $version }
asteria-common-desktop-about-licence-ffmpeg-vips = Asteria is free software under GPL-3.0-or-later. Packages for Windows and macOS include ffmpeg (GPLv3) and, on Windows, libvips (LGPL); their licences and sources are listed in tools/LICENSES beside the program (on macOS, in the app's Contents/Resources).
asteria-common-desktop-about-licence-vips = Asteria is free software under GPL-3.0-or-later. The Windows package includes libvips (LGPL); its licence and sources are listed in tools/LICENSES beside the program.
asteria-common-desktop-about-licence-none = Asteria is free software under GPL-3.0-or-later.
