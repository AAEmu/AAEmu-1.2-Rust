#pragma once
#include <stdint.h>
#include <string>

struct IDXGISwapChain;

// Constant-frame-rate video of the client's back buffer, encoded by an ffmpeg child process.
// See videocore.h for the cadence rules and docs/client/LUA-DRIVER.md "Skill trace and video".
namespace video {
// Pipe op 'M'. "start\n<fps>\n<ffmpeg or empty>\n<output path>", "stop", "status".
// Reply = status byte + JSON text ('0' ok, '2' driver error). stop returns the sidecar JSON.
std::string control(const std::string& command);
// Present hook, render thread, before the real Present. width/height are the swap chain's
// back buffer size. Costs one atomic load when no recording is active.
void on_present(IDXGISwapChain* chain, unsigned width, unsigned height);
// Driver unload: stops a running recording.
void remove();
std::string info();
}
