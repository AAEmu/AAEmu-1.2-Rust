// AAEmu client extension layer (see rust/docs/client/CLIENT-EXTENSIONS.md).
// Compiled into both the diagnostic and the player (AADRIVER_PLAYER) driver builds.
#pragma once
#include <windows.h>
#include <string>

typedef struct lua_State lua_State;

namespace ext {

typedef void (*LogFn)(const char* line);

struct Host {
    LogFn       log;            // one finished line, no trailing newline (may be NULL)
    BYTE*       css;            // CryScriptSystem.dll base (Lua C API is resolved from it)
    const char* driverVersion;  // reported by AAEmuExt.Info().driver and in ext.hello
    const char* bundle;         // embedded Lua bundle ("--@@ext chunk <name>" sections)
    size_t      bundleSize;
};

// Remember the host. Nothing is touched in the client yet.
void init(const Host& host);

// Main thread, once per frame, with the UI script state (may be NULL while it does not exist).
// Verifies the x2game.dll build, claims the receive slot, (re)installs the natives and the
// embedded Lua when the UI state was rebuilt, delivers queued messages and drives the Lua tick.
void tick(lua_State* ui);

// Restore the client's tables (diagnostic build 'X' unload).
void remove();

// One-line status for the driver ping.
std::string info();

}  // namespace ext
