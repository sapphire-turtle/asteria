# asteria-photos' strings. Keys are `asteria-photos-<area>-<what>`.

# The rail.
asteria-photos-screen-library = Library
asteria-photos-screen-settings = Settings

# The library screen.
asteria-photos-library-root = Library
asteria-photos-library-scanning = Scanning ({ $count } items)
asteria-photos-library-loading = Loading...
asteria-photos-library-empty = This folder is empty.
asteria-photos-library-failed = This folder could not be loaded: { $reason }
asteria-photos-library-retry = Try again
asteria-photos-library-connecting = Connecting to { $name }...
asteria-photos-library-connect-failed = Could not connect to { $name }: { $reason }
asteria-photos-library-no-servers = Add a server to see your photos.
asteria-photos-library-no-active = Choose a server to see its photos.
asteria-photos-library-open-servers = Server settings

# The server switcher, on the status bar (or the bottom bar on a narrow screen).
asteria-photos-status-none = No server
asteria-photos-status-connected = Connected
asteria-photos-status-connecting = Connecting
asteria-photos-status-failed = Not connected: { $reason }
asteria-photos-status-degraded = Not responding: { $reason }
asteria-photos-status-manage = Manage servers...
asteria-photos-status-hover = { $name }: { $status }

# Saving the server list.
asteria-photos-save-failed = The server list could not be saved: { $reason }
asteria-photos-load-failed = The server list could not be read, so changes will not be saved: { $reason }

# The detail pane (the side pane beside the library).
asteria-photos-detail-none = Select a photo to see its details here.
asteria-photos-detail-other-screen = The details of the selected photo show here while you browse the library.
asteria-photos-detail-open = View full screen
asteria-photos-detail-folder = Folder
asteria-photos-detail-size = Size
asteria-photos-detail-modified = Modified
asteria-photos-detail-video = Video: only a still image is shown.
asteria-photos-detail-preview-failed = The preview could not be loaded: { $reason }
asteria-photos-detail-read-only = This item cannot be edited.
asteria-photos-detail-read-only-reason = Reason: { $reason }
asteria-photos-detail-busy = The server was busy with another change, so this one was not saved. Please try again.
asteria-photos-detail-save-failed = The change could not be saved: { $reason }
asteria-photos-detail-saving = Saving...
asteria-photos-detail-rating = Rating
asteria-photos-detail-rating-none = Not rated
asteria-photos-detail-rating-set = { $stars ->
    [one] Rate 1 star
   *[other] Rate { $stars } stars
}
asteria-photos-detail-rating-clear = Clear the rating
asteria-photos-detail-label = Color label
asteria-photos-detail-label-none = None
asteria-photos-detail-label-red = Red
asteria-photos-detail-label-yellow = Yellow
asteria-photos-detail-label-green = Green
asteria-photos-detail-label-blue = Blue
asteria-photos-detail-label-purple = Purple
asteria-photos-detail-subjects = Keywords
asteria-photos-detail-subjects-add = Add a keyword
asteria-photos-detail-hierarchical = Hierarchical tags
asteria-photos-detail-hierarchical-add = Add a tag, such as Places|Japan
asteria-photos-detail-tags-empty = None
asteria-photos-detail-tag-add = Add
asteria-photos-detail-tag-remove = Remove “{ $tag }”
asteria-photos-detail-description = Description
asteria-photos-detail-description-hint = Saved when you leave the field
asteria-photos-detail-description-empty = No description

# The full-screen preview.
asteria-photos-viewer-close = Close (Esc)
asteria-photos-viewer-previous = Previous (←)
asteria-photos-viewer-next = Next (→)
asteria-photos-viewer-details = Details
asteria-photos-viewer-position = { $index } / { $total }

# The Uploads screen.
asteria-photos-screen-uploads = Uploads
asteria-photos-uploads-unavailable = Connect to a server to upload files.
asteria-photos-uploads-empty = Drop photos or folders on the window to send them to the server's Inbox.
asteria-photos-uploads-starting = Opening the upload queue...
asteria-photos-uploads-waiting = The server is not answering, so uploads are paused and will try again: { $reason }
asteria-photos-uploads-stopped = Uploads have stopped: { $reason }
asteria-photos-uploads-queue-unreadable = The upload queue could not be read, so nothing is sent: { $reason }
asteria-photos-uploads-clear-finished = Clear finished
asteria-photos-uploads-in-library = { $count } already in the library
asteria-photos-uploads-keep-all = Save all
asteria-photos-uploads-discard-all = Discard all
asteria-photos-uploads-keep = Save
asteria-photos-uploads-discard = Discard
asteria-photos-uploads-retry = Try again
asteria-photos-uploads-cancel = Cancel
asteria-photos-uploads-existing = Already in the library:
asteria-photos-uploads-excluded = { $count } files in the last drop were left out (hidden or not photos and videos).
asteria-photos-uploads-warn-sidecar = Not sent, it matches several files: { $path }
asteria-photos-uploads-warn-unreadable = Could not read { $path }: { $reason }
asteria-photos-uploads-state-hashing = Reading
asteria-photos-uploads-state-checking = Checking the library
asteria-photos-uploads-state-awaiting = Already in the library
asteria-photos-uploads-state-uploading = Uploading
asteria-photos-uploads-state-committing = Saving
asteria-photos-uploads-state-done = Done
asteria-photos-uploads-state-discarded = Discarded
asteria-photos-uploads-state-failed = Failed: { $reason }

# Dropping files on the window, and the status bar's count.
asteria-photos-drop-overlay = Send to Inbox
asteria-photos-drop-refused = Not connected to a server, so the files were not sent.
asteria-photos-status-uploads = ↑ { $done } / { $total }
asteria-photos-status-uploads-awaiting = ↑ { $done } / { $total } · { $count } to decide
asteria-photos-status-uploads-hover = Open the uploads
asteria-photos-drop-starting = Still connecting; try again in a moment.
asteria-photos-drop-no-data = Uploads are not available: this app has no place to keep the queue.
