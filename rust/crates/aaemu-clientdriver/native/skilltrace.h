#pragma once
#include <string>

// Native skill trace: records animation, particle, sound, key press and frame events of the
// original client as `skillparity/v1` JSON rows in a bounded ring buffer (see tracecore.h and
// docs/client/LUA-DRIVER.md "Skill trace and video").
namespace skilltrace {
// Pipe op 'R'. Commands: "start [kinds]", "stop", "drain", "status". Reply = status byte + text
// ('0' ok, '2' driver error); drain returns the JSONL text itself.
std::string control(const std::string& command);
// Present hook: counts frames and writes `frame` rows when that kind is requested.
void on_present(unsigned frame);
// Input path, main thread: a key was injected. modifier_mask uses the native bits (0x11 ctrl,
// 0x22 shift, 0x44 alt).
void on_press(const char* key, int modifier_mask);
// Driver unload: restores every patched vtable slot.
void remove();
std::string info();
}
