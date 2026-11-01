# asteria-framework-desktop's strings. Keys are `asteria-desktop-<area>-<what>`.

# The server on this computer, as the servers list names it.
asteria-desktop-this-computer = This computer

# First run: where the library is.
asteria-desktop-where-title = Where is your library?
asteria-desktop-where-this-computer-note = Choose a folder on this computer. A server runs here and the app connects to it.
asteria-desktop-where-other = Another server
asteria-desktop-where-other-note = The library is served by another computer. Add that server in Servers.
asteria-desktop-where-later = Later
asteria-desktop-where-later-note = Set it up later, in Settings.

# First run: setting up the server on this computer.
asteria-desktop-setup-title = Set up a library on this computer
asteria-desktop-setup-intro = Choose the library's folder. A folder that is already a library is used as it is; any other becomes one.
asteria-desktop-setup-choose = Choose a folder…
asteria-desktop-setup-picker-title = The library's folder
asteria-desktop-setup-running = Setting up { $path }…
asteria-desktop-setup-done = The library at { $path } is ready. The app connects to it at { $url }.
asteria-desktop-setup-failed = The library could not be set up: { $error }
asteria-desktop-setup-stopped = the setup stopped unexpectedly
asteria-desktop-setup-try-again = Try again
asteria-desktop-setup-other-folder = Choose another folder

# First run: the background service.
asteria-desktop-service-question = Keep the server running in the background?
asteria-desktop-service-reason = The app shows the library through the server. In the background it starts when you log in and keeps watching the library.
asteria-desktop-not-running = The server is not running. To start it, run:

# The external tools.
asteria-desktop-tools-title = External tools
asteria-desktop-tools-intro = The server makes thumbnails and smaller copies with these programs. Without them it still runs, just without those.
asteria-desktop-tools-vips-purpose = Image thumbnails.
asteria-desktop-tools-ffmpeg-purpose = Video thumbnails, and smaller copies of music for offline use.
asteria-desktop-tools-found = Found: { $path }
asteria-desktop-tools-missing = Not found.
asteria-desktop-tools-linux = Install the package { $package } with your distribution's package manager.
asteria-desktop-tools-macos = Install it with Homebrew: brew install { $package }
asteria-desktop-tools-windows-vips = Download libvips for Windows (github.com/libvips/build-win64-mxe/releases), and add its bin folder to PATH.
asteria-desktop-tools-windows-ffmpeg = Download FFmpeg for Windows (ffmpeg.org/download.html), and add its bin folder to PATH.
asteria-desktop-tools-env = Or set { $var } to the program's path.
asteria-desktop-tools-check-again = Check again

# The "Local server" settings page.
asteria-desktop-page-label = Local server
asteria-desktop-page-not-set-up = No local server is set up yet.
asteria-desktop-page-set-up = Set up a local server
asteria-desktop-page-unreadable = The local server's config could not be read: { $error }
asteria-desktop-page-library = Library
asteria-desktop-page-listen = Listening on
asteria-desktop-page-config = Config file
asteria-desktop-page-listen-note = To accept connections from other devices, change listen in the config file.
asteria-desktop-page-service = Background server
asteria-desktop-page-service-note = Without the background service, start the server yourself:
asteria-desktop-page-keys = API keys
asteria-desktop-page-keys-failed = The API keys could not be opened: { $error }
