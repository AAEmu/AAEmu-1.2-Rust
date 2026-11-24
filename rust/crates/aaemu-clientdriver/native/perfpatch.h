#pragma once
// Client performance and reliability patch set for the r208022 client (perfpatch.cpp).
// Every patch has an id, a default and a switch; status is kept per id and logged.
#include <string>

namespace perfpatch {
typedef void (*Logger)(const char* fmt, ...);

// DllMain (DLL_PROCESS_ATTACH, loader lock held): read the switches, register the module-load
// notification and start the worker thread. Modules that are already loaded only get the patches
// that are safe on running code.
void attach(HMODULE self, Logger log);
// The resolved switch of a patch id (false before attach). The driver's older start-up paths
// (pak read-ahead "optimize", the CPU calibration priorities) run only when their id is on.
bool enabled(const char* id);
// Detach ('X'): restore what can be restored on running code (IAT slots, one-byte patches).
void remove();
// " perf=<applied>/<enabled>" for the ping reply.
std::string info();
// Pipe op 'Q': "status" -> one line per patch: id state reason.
std::string control(const std::string& cmd);
// Lua chunk for the first script tick: writes the patch status into ArcheAge.log and applies the
// requested CVars (-aaemu-cvars=). Empty when there is nothing left to do. `result` is the reply of
// the previous chunk ("0..." / "1..."), empty on the first call.
std::string first_tick_lua(const std::string& result);
}
