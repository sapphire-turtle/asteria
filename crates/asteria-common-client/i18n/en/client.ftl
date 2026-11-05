# asteria-common-client's strings. Keys are `asteria-common-client-<area>-<what>`.

# The thumbnail grid.
asteria-common-client-grid-failed = The thumbnail could not be loaded: { $reason }
asteria-common-client-grid-video = Video
asteria-common-client-grid-read-only = Read-only: { $reason }
asteria-common-client-grid-rating = Rated { $stars } of 5

# The servers settings page.
asteria-common-client-servers-title = Servers
asteria-common-client-servers-empty = No servers yet. Add one to get started.
asteria-common-client-servers-in-use = In use
asteria-common-client-servers-use = Use this server
asteria-common-client-servers-add = Add a server
asteria-common-client-servers-edit = Edit
asteria-common-client-servers-delete = Delete
asteria-common-client-servers-delete-confirm = Delete "{ $name }"?
asteria-common-client-servers-delete-yes = Delete
asteria-common-client-servers-cancel = Cancel
asteria-common-client-servers-form-add = New server
asteria-common-client-servers-form-edit = Edit server
asteria-common-client-servers-name = Name
asteria-common-client-servers-url = Address
asteria-common-client-servers-token = Access token
asteria-common-client-servers-token-show = Show
asteria-common-client-servers-token-hide = Hide
asteria-common-client-servers-save = Save
asteria-common-client-servers-test = Test connection
asteria-common-client-servers-testing = Connecting...
asteria-common-client-servers-test-ok = Connected. { $count } items, owner: { $owner }
asteria-common-client-servers-test-failed = Could not connect: { $reason }
asteria-common-client-servers-error-name = Enter a name.
asteria-common-client-servers-error-url = Enter an http:// or https:// address, such as http://192.168.1.10:7810.
asteria-common-client-servers-error-unreachable = the server cannot be reached ({ $detail })
asteria-common-client-servers-error-unauthorized = the server did not accept the access token
asteria-common-client-servers-error-app-mismatch = this server is for a different app
asteria-common-client-servers-error-api-mismatch = the server and this app use incompatible versions (app { $client }, server { $server })
asteria-common-client-servers-error-busy = the server is busy; it asks to try again in { $seconds } s
asteria-common-client-servers-error-unprocessable = the server cannot process this file
asteria-common-client-servers-error-not-found = it is no longer on the server
asteria-common-client-servers-error-library-changed = the server now serves a different library
asteria-common-client-servers-error-server = the server reported an error
asteria-common-client-servers-error-protocol = the server's answer could not be understood
asteria-common-client-servers-error-local = something went wrong on this device

# The server switcher in the status bar.
asteria-common-client-status-none = No server
asteria-common-client-status-connected = Connected
asteria-common-client-status-connecting = Connecting
asteria-common-client-status-failed = Not connected: { $reason }
asteria-common-client-status-degraded = Not responding: { $reason }
asteria-common-client-status-manage = Manage servers...
asteria-common-client-status-hover = { $name }: { $status }
