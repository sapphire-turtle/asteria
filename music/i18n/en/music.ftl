# asteria-music's strings. Keys are `asteria-music-<area>-<what>`.

# The rail.
asteria-music-screen-albums = Albums
asteria-music-screen-artists = Artists
asteria-music-screen-playlists = Playlists
asteria-music-screen-queue = Queue
asteria-music-screen-settings = Settings

# The connection.
asteria-music-library-connecting = Connecting to { $name }...
asteria-music-library-connect-failed = Could not connect to { $name }: { $reason }
asteria-music-library-no-servers = Add a server to see your music.
asteria-music-library-no-active = Choose a server to see its music.
asteria-music-library-open-servers = Server settings
asteria-music-library-retry = Try again
asteria-music-library-loading = Loading...
asteria-music-library-failed = This could not be loaded: { $reason }

# Saving the server list.
asteria-music-save-failed = The server list could not be saved: { $reason }
asteria-music-load-failed = The server list could not be read, so changes will not be saved: { $reason }

# The lists.
asteria-music-albums-empty = There are no albums yet.
asteria-music-artists-empty = There are no artists yet.
asteria-music-playlists-empty = There are no playlists yet.
asteria-music-artists-albums = { $count ->
    [one] 1 album
   *[other] { $count } albums
}

# An opened album, artist or playlist.
asteria-music-detail-back = Back
asteria-music-detail-album-tracks = { $count ->
    [one] 1 track
   *[other] { $count } tracks
}
asteria-music-detail-own-albums = Albums
asteria-music-detail-appears-on = Appears on
asteria-music-detail-truncated = This playlist is long; only its first entries are shown.

# The track table.
asteria-music-tracks-disc = Disc
asteria-music-tracks-number = #
asteria-music-tracks-title = Title
asteria-music-tracks-artist = Artist
asteria-music-tracks-length = Length
asteria-music-tracks-moved = moved
asteria-music-tracks-moved-hover = This file was moved; it was found at its new place.
asteria-music-tracks-missing-hover = No file was found for { $path }.
asteria-music-tracks-play-next = Play next
asteria-music-tracks-append = Add to queue

# The playback bar.
asteria-music-bar-previous = Previous
asteria-music-bar-next = Next
asteria-music-bar-play = Play
asteria-music-bar-pause = Pause
asteria-music-bar-volume = Volume
asteria-music-bar-buffering = Buffering...
asteria-music-bar-preparing = Preparing...
asteria-music-bar-error-unplayable = "{ $title }" could not be played and was skipped: { $reason }
asteria-music-bar-error-network = The server could not be reached, so playback paused: { $reason }
asteria-music-bar-error-unauthorized = The server did not accept the token, so playback paused.
asteria-music-bar-error-output = The audio device failed, so playback paused: { $reason }
asteria-music-bar-error-too-many = Several tracks in a row could not be played, so playback stopped.

# The queue.
asteria-music-queue-empty = The queue is empty. Play an album or add tracks to it.
asteria-music-queue-up = Move up
asteria-music-queue-down = Move down
asteria-music-queue-remove = Remove from the queue

# The playback settings.
asteria-music-playback-title = Playback
asteria-music-playback-low-quality = Play in low quality
asteria-music-playback-low-quality-hint = Takes effect from the next track. For a slow connection, tracks are streamed from the server's offline rendition, which is smaller.
asteria-music-playback-save-failed = The playback settings could not be saved: { $reason }
