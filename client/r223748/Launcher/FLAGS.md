# r223748 launcher options

The EXE targets the stock Trion/Steam r223748 client, dated 2015-03-18. Connection
arguments were recovered from its launcher parser (`x2game` `FUN_3951f8d0`):

```text
-t +auth_ip <IPv4> -auth_port <port> -handle <MAP:EVENT> -lang en_us
```

The launcher supplies the shared-memory authentication ticket and appends the
existing `+acpxmk` argument. No Steam session is required without `--steam`.
Username/password are carried in the ticket, never in the process command line.
IP/port and all generated CVar values are validated before launch.

| UI option | Implementation |
|---|---|
| Windowed / fullscreen | `+r_Fullscreen 0` / `1` |
| Resolution | `+r_Width <width> +r_Height <height>` |
| VSync on / off | `+r_VSync 1` / `0` |
| Mute all sound | `+s_SoundEnable 0`; unchecked restores `1` |
| Mute music | `+s_MusicEnable 0`; unchecked restores `1` |
| Start minimized | Win32 `STARTUPINFO.wShowWindow = SW_SHOWMINNOACTIVE` |
| Use game setting | Omits the corresponding display arguments and config edits |

Display choices also update only the selected renderer keys in the user's
Documents/ArcheAge/system.cfg, because this client restores user renderer settings
during initialization. Other preferences are preserved; the previous file is
saved as system.cfg.aaemu.bak. The client may restore its own window state later.

## Evidence and limits

Local Ghidra analysis of the decrypted r223748 CrySystem module:

- `FUN_36525330` registers r_Width, r_Height and r_Fullscreen, clamps width/height
  to at least 1024/768, and enumerates Windows display modes.
- `FUN_365489d0` tests the requested display mode with ChangeDisplaySettingsExA.
  If it fails, the client chooses an enumerated mode. Consequently the launcher
  offers 1280x768 rather than 1280x720. The UI filters presets with the same
  Windows display-mode test, so unsupported monitor/driver modes are hidden.
  A mode is checked again before launch in case the display changed.
- `x2game` contains r_VSync; its fullscreen toggle is `FUN_3951ee20`.
- The stock, readable crysoundsystem.dll contains both s_SoundEnable and
  s_MusicEnable, with their on/off registration/help strings. These options do
  not require a replacement DLL.

The compiled launcher was tested with the actual client against an isolated local
login listener: its first Trion login packet reached the selected IPv4/port,
all three signature-checked runtime patches applied, and its window measured
1600x900 when that mode was selected. This is a launch/renderer check, not an
additional gameplay compatibility claim. Existing native-server gameplay
verification is documented in Docs/r223748.md. Audio CVars are confirmed in this
client's sound module; no claim of an acoustic measurement is made.

Primary CryEngine documentation also describes
[command-line +CVar syntax](https://www.cryengine.com/docs/static/engines/cryengine-3/categories/1114113/pages/12746775),
[renderer CVars](https://www.cryengine.com/docs/static/engines/cryengine-3/categories/1638401/pages/1605722),
and [sound CVars](https://www.cryengine.com/docs/static/engines/cryengine-3/categories/1114113/pages/1048815).
Local client behavior takes precedence over generic engine defaults.

The optional `--steam` switch and unverified splash/renderer/debug flags are not
exposed as basic options. The launcher supplies the three required in-memory
compatibility patches automatically. Artwork provenance and exact generation
prompts are in Assets/PROMPTS.txt; generated originals and derived ICO/JPEG assets
are included in source.
