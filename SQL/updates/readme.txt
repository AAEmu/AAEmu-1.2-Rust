Add each MySQL schema change here and apply the same change to the appropriate
base schema in SQL/aaemu_login.sql or SQL/aaemu_game.sql.

Use YYYY-MM-DD_aaemu_login_<description>.sql for login changes and
YYYY-MM-DD_aaemu_game_<description>.sql for game changes. Each server matches
its own files and processes them in filename order.

The server records installed scripts in its schema's updates table. If the
updates table is missing, the server creates it and marks the matching scripts
as installed, assuming the current base schema was used. Initialize new schemas
from the current base SQL before starting either server.

For an existing updates table, Connections:AutoApplyUpdates=true applies
pending scripts on startup. With it disabled, the server lists pending files
and asks before applying them.
