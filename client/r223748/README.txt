ArcheAge 1.2.5.0 r223748 - AAEmu launcher

Extract the complete client bundle and double-click Play.cmd inside ArcheAge.
Enter the server's IPv4 address, login port (normally 1237), account and password.
Select the game server and your character normally. No Steam login is required.
Edit server.json to change the default address, port and username. Passwords are
not saved. Requires Windows PowerShell 5.1 and .NET Framework (included in Windows).

Use the matching feat/client-r223748 Rust server branch, ClientRevision=223748.
The server administrator must advertise a reachable game/stream address; changing
only the login IP cannot override a server that advertises 127.0.0.1. Default public
ports: TCP 1237 login, 1239 game, 1250 stream. Never expose the internal login port.

The launcher verifies the three exact r223748 executable hashes, then applies
HackShield/no-browser and AAEmu no-crypto patches to the new process's memory.
It does not modify client binaries or game_pak. A signature mismatch fails closed.
Only the process it launched can be terminated if patching fails.

DirectX helper DLLs, when bundled, are loaded from Launcher\DirectX. Otherwise
install Microsoft's DirectX June 2010 runtime. Launch errors appear in the window;
game logs remain in Documents\ArcheAge\ArcheAge.log.

This is the compatibility implementation tested with world entry, movement, plain
chat, NPC quests, mining and combat. Full feature coverage is not established.
Unknown game packets are withheld. Item hyperlinks, batch actor movements and exact
mail-total semantics need further work. The game data schema remains r208022.

The public repository contains launcher/patch source only. The full client bundle
is assembled locally from the user's own r223748 archive; no credentials, research
dumps or private runtime/server files are included.
