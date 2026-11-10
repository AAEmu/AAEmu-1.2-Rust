using System;
using System.Collections.Generic;
using System.IO;
using System.Net;
using System.Net.Sockets;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;
using System.Threading;

public static class ClientRuntime {
    // ---------------- Win32 ----------------
    [StructLayout(LayoutKind.Sequential)] struct SECURITY_ATTRIBUTES { public int nLength; public IntPtr lpSecurityDescriptor; public int bInheritHandle; }
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)] struct STARTUPINFO {
        public int cb; public string lpReserved; public string lpDesktop; public string lpTitle;
        public int dwX, dwY, dwXSize, dwYSize, dwXCountChars, dwYCountChars, dwFillAttribute, dwFlags;
        public short wShowWindow, cbReserved2; public IntPtr lpReserved2, hStdInput, hStdOutput, hStdError; }
    [StructLayout(LayoutKind.Sequential)] struct STARTUPINFOEX { public STARTUPINFO StartupInfo; public IntPtr lpAttributeList; }
    [StructLayout(LayoutKind.Sequential)] struct PROCESS_INFORMATION { public IntPtr hProcess, hThread; public int dwProcessId, dwThreadId; }
    [StructLayout(LayoutKind.Sequential)] struct MODULEINFO { public IntPtr lpBaseOfDll; public uint SizeOfImage; public IntPtr EntryPoint; }

    [DllImport("kernel32", SetLastError = true, CharSet = CharSet.Unicode)] static extern IntPtr CreateFileMappingW(IntPtr hFile, ref SECURITY_ATTRIBUTES sa, uint prot, uint hi, uint lo, string name);
    [DllImport("kernel32", SetLastError = true)] static extern IntPtr MapViewOfFile(IntPtr h, uint access, uint hi, uint lo, UIntPtr n);
    [DllImport("kernel32", SetLastError = true)] static extern bool UnmapViewOfFile(IntPtr p);
    [DllImport("kernel32", SetLastError = true, CharSet = CharSet.Unicode)] static extern IntPtr CreateEventW(ref SECURITY_ATTRIBUTES sa, bool manual, bool initial, string name);
    [DllImport("kernel32", SetLastError = true)] static extern uint WaitForSingleObject(IntPtr h, uint ms);
    [DllImport("kernel32", SetLastError = true)] static extern bool CloseHandle(IntPtr h);
    [DllImport("kernel32", SetLastError = true)] static extern bool InitializeProcThreadAttributeList(IntPtr list, int count, int flags, ref IntPtr size);
    [DllImport("kernel32", SetLastError = true)] static extern bool UpdateProcThreadAttribute(IntPtr list, uint flags, IntPtr attr, IntPtr value, IntPtr size, IntPtr prev, IntPtr ret);
    [DllImport("kernel32", SetLastError = true)] static extern void DeleteProcThreadAttributeList(IntPtr list);
    [DllImport("kernel32", SetLastError = true, CharSet = CharSet.Unicode)] static extern bool CreateProcessW(string app, StringBuilder cmd, IntPtr pa, IntPtr ta, bool inherit, uint flags, IntPtr env, string cwd, ref STARTUPINFOEX si, out PROCESS_INFORMATION pi);
    [DllImport("kernel32", SetLastError = true)] static extern IntPtr OpenProcess(uint access, bool inherit, int pid);
    [DllImport("kernel32", SetLastError = true)] static extern bool ReadProcessMemory(IntPtr h, IntPtr addr, byte[] buf, IntPtr size, out IntPtr read);
    [DllImport("kernel32", SetLastError = true)] static extern bool GetExitCodeProcess(IntPtr h, out uint code);
    [DllImport("psapi", SetLastError = true)] static extern bool EnumProcessModulesEx(IntPtr h, [Out] IntPtr[] mods, int cb, out int needed, uint filter);
    [DllImport("psapi", SetLastError = true, CharSet = CharSet.Unicode)] static extern uint GetModuleBaseNameW(IntPtr h, IntPtr mod, StringBuilder name, int size);
    [DllImport("psapi", SetLastError = true)] static extern bool GetModuleInformation(IntPtr h, IntPtr mod, out MODULEINFO mi, int cb);

    // ---------------- Ticket (mirrors aaemu-launcher lib.rs) ----------------
    static byte[] Rc4(byte[] key, byte[] data) {
        var s = new byte[256]; for (int i = 0; i < 256; i++) s[i] = (byte)i;
        int j = 0; for (int i = 0; i < 256; i++) { j = (j + s[i] + key[i % key.Length]) & 255; var t = s[i]; s[i] = s[j]; s[j] = t; }
        var o = new byte[data.Length]; int x = 0; j = 0;
        for (int k = 0; k < data.Length; k++) { x = (x + 1) & 255; j = (j + s[x]) & 255; var t = s[x]; s[x] = s[j]; s[j] = t; o[k] = (byte)(data[k] ^ s[(s[x] + s[j]) & 255]); }
        return o;
    }
    public static string Sha256Hex(string s) {
        using (var h = SHA256.Create()) { var d = h.ComputeHash(Encoding.UTF8.GetBytes(s)); var sb = new StringBuilder(); foreach (var b in d) sb.Append(b.ToString("x2")); return sb.ToString(); }
    }
    public static byte[] TicketMapping(string user, string password) {
        string xml = "<?xml version=\"1.0\" encoding=\"UTF - 8\" standalone=\"yes\"?><authTicket version = \"1.2\"><storeToken>1</storeToken><username>"
            + user + "</username><password>" + Sha256Hex(password) + "</password></authTicket>";
        var plain = Encoding.UTF8.GetBytes("TFIRdGVzdA==\n" + xml);
        var key = new byte[8]; using (var r = RandomNumberGenerator.Create()) r.GetBytes(key);
        var enc = Rc4(key, plain);
        var m = new byte[12 + enc.Length];
        Buffer.BlockCopy(key, 0, m, 0, 8); Buffer.BlockCopy(BitConverter.GetBytes(enc.Length), 0, m, 8, 4); Buffer.BlockCopy(enc, 0, m, 12, enc.Length);
        return m;
    }

    public class Launched { public int Pid; public IntPtr Process, Thread, Map, Event; public string CommandLine; }

    // Starts the client like aaemu-launcher: inheritable ticket mapping + event, handle list, cwd = bin32.
    // Not suspended (no driver injection). showCmd: 7 = SW_SHOWMINNOACTIVE.
    public static Launched Launch(string exe, string argsTemplate, string user, string password, short showCmd) {
        var sa = new SECURITY_ATTRIBUTES { nLength = Marshal.SizeOf(typeof(SECURITY_ATTRIBUTES)), bInheritHandle = 1 };
        var bytes = TicketMapping(user, password);
        string suffix = System.Diagnostics.Process.GetCurrentProcess().Id + "-" + Guid.NewGuid().ToString("N").Substring(0, 16);
        IntPtr map = CreateFileMappingW(new IntPtr(-1), ref sa, 0x04, 0, (uint)bytes.Length, "archeage_auth_ticket_map-" + suffix);
        if (map == IntPtr.Zero) throw new Exception("CreateFileMapping " + Marshal.GetLastWin32Error());
        IntPtr view = MapViewOfFile(map, 0xF001F, 0, 0, new UIntPtr((uint)bytes.Length));
        Marshal.Copy(bytes, 0, view, bytes.Length); UnmapViewOfFile(view);
        IntPtr ev = CreateEventW(ref sa, true, false, "archeage_auth_ticket_event-" + suffix);
        if (ev == IntPtr.Zero) throw new Exception("CreateEvent " + Marshal.GetLastWin32Error());

        IntPtr size = IntPtr.Zero; InitializeProcThreadAttributeList(IntPtr.Zero, 1, 0, ref size);
        IntPtr list = Marshal.AllocHGlobal(size);
        if (!InitializeProcThreadAttributeList(list, 1, 0, ref size)) throw new Exception("InitAttr " + Marshal.GetLastWin32Error());
        IntPtr handles = Marshal.AllocHGlobal(IntPtr.Size * 2);
        Marshal.WriteIntPtr(handles, 0, map); Marshal.WriteIntPtr(handles, IntPtr.Size, ev);
        if (!UpdateProcThreadAttribute(list, 0, new IntPtr(0x20002), handles, new IntPtr(IntPtr.Size * 2), IntPtr.Zero, IntPtr.Zero)) throw new Exception("UpdateAttr " + Marshal.GetLastWin32Error());

        string handleArg = ((uint)map.ToInt64()).ToString("X8") + ":" + ((uint)ev.ToInt64()).ToString("X8");
        string cmdLine = "\"" + exe + "\" " + argsTemplate.Replace("{HANDLE}", handleArg);
        var si = new STARTUPINFOEX(); si.StartupInfo.cb = Marshal.SizeOf(typeof(STARTUPINFOEX));
        si.StartupInfo.dwFlags = 1; si.StartupInfo.wShowWindow = showCmd; si.lpAttributeList = list;
        PROCESS_INFORMATION pi;
        bool ok = CreateProcessW(exe, new StringBuilder(cmdLine), IntPtr.Zero, IntPtr.Zero, true, 0x400 | 0x80000, IntPtr.Zero, Path.GetDirectoryName(exe), ref si, out pi);
        DeleteProcThreadAttributeList(list); Marshal.FreeHGlobal(list); Marshal.FreeHGlobal(handles);
        if (!ok) throw new Exception("CreateProcess " + Marshal.GetLastWin32Error());
        return new Launched { Pid = pi.dwProcessId, Process = pi.hProcess, Thread = pi.hThread, Map = map, Event = ev, CommandLine = cmdLine };
    }
    public static bool TicketRead(Launched l) { return WaitForSingleObject(l.Event, 0) == 0; }
    public static string ExitState(Launched l) { uint c; GetExitCodeProcess(l.Process, out c); return c == 259 ? "running" : ("exited 0x" + c.ToString("X")); }

    // ---------------- Dialogs of our own client ----------------
    delegate bool EnumProc(IntPtr hwnd, IntPtr lp);
    [DllImport("user32")] static extern bool EnumWindows(EnumProc cb, IntPtr lp);
    [DllImport("user32")] static extern bool EnumChildWindows(IntPtr parent, EnumProc cb, IntPtr lp);
    [DllImport("user32")] static extern uint GetWindowThreadProcessId(IntPtr hwnd, out int pid);
    [DllImport("user32", CharSet = CharSet.Unicode)] static extern int GetClassNameW(IntPtr hwnd, StringBuilder sb, int n);
    [DllImport("user32", CharSet = CharSet.Unicode)] static extern int GetWindowTextW(IntPtr hwnd, StringBuilder sb, int n);
    [DllImport("user32")] static extern IntPtr GetDlgItem(IntPtr hwnd, int id);
    [DllImport("user32")] static extern bool PostMessageW(IntPtr hwnd, uint msg, IntPtr wp, IntPtr lp);
    [DllImport("user32")] static extern bool IsWindowVisible(IntPtr hwnd);
    static readonly HashSet<long> reported = new HashSet<long>();

    // Reports every dialog (#32770) owned by pid once. Clicks Yes only on the loader's
    // second-instance prompt ("... start another one?"); every other dialog is left alone.
    public static List<string> HandleDialogs(int pid) {
        var r = new List<string>();
        EnumWindows((h, lp) => {
            int p; GetWindowThreadProcessId(h, out p);
            if (p != pid) return true;
            var cls = new StringBuilder(64); GetClassNameW(h, cls, 64);
            if (cls.ToString() != "#32770") return true;
            var title = new StringBuilder(512); GetWindowTextW(h, title, 512);
            var text = new StringBuilder();
            EnumChildWindows(h, (c, l2) => { var cc = new StringBuilder(64); GetClassNameW(c, cc, 64); var t = new StringBuilder(1024); GetWindowTextW(c, t, 1024); if (t.Length > 0) text.Append("[" + cc + ":" + t + "] "); return true; }, IntPtr.Zero);
            string all = title + " | " + text;
            bool secondInstance = all.IndexOf("another", StringComparison.OrdinalIgnoreCase) >= 0 && GetDlgItem(h, 6) != IntPtr.Zero;
            if (!reported.Contains(h.ToInt64())) { reported.Add(h.ToInt64()); r.Add("dialog: " + all + (secondInstance ? " -> clicking Yes" : " -> left alone")); }
            if (secondInstance) PostMessageW(h, 0x0111, new IntPtr(6), GetDlgItem(h, 6));
            return true;
        }, IntPtr.Zero);
        return r;
    }

    // ---------------- Runtime patcher (in-memory only; files untouched) ----------------
    [DllImport("kernel32", SetLastError = true)] static extern bool WriteProcessMemory(IntPtr h, IntPtr addr, byte[] buf, IntPtr size, out IntPtr written);
    [DllImport("kernel32", SetLastError = true)] static extern bool VirtualProtectEx(IntPtr h, IntPtr addr, IntPtr size, uint prot, out uint old);
    [DllImport("kernel32", SetLastError = true)] static extern bool FlushInstructionCache(IntPtr h, IntPtr addr, IntPtr size);
    [DllImport("kernel32", SetLastError = true)] static extern bool TerminateProcess(IntPtr h, uint code);

    public class PatchSpec {
        public string Name; public long CheckAddr; public byte[] Expect; public long PatchAddr; public byte[] Patch;
        // Optional module-relative addresses, for DLLs whose load address varies per launch.
        public string ModuleName; public bool BeforeTicket = true;
        public bool Done; public double AppliedMs = -1; public string Error;
    }
    public static volatile string PatcherState = "idle";
    public static double PatcherKilledAtMs = -1;

    // Spins until every spec is applied: waits for the decrypted bytes (Expect at CheckAddr), then writes
    // Patch at PatchAddr and verifies it. Kill switch: if the ticket event fires before all specs are
    // applied, the client is terminated at once (the HackShield path runs right after the ticket read).
    public static void StartPatcher(Launched l, PatchSpec[] specs, int timeoutMs) {
        var sw = System.Diagnostics.Stopwatch.StartNew();
        PatcherState = "waiting";
        var t = new Thread(() => {
            while (sw.ElapsedMilliseconds < timeoutMs) {
                bool all = true;
                foreach (var s in specs) {
                    if (s.Done) continue;
                    all = false;
                    long moduleBase = 0;
                    if (!String.IsNullOrEmpty(s.ModuleName)) {
                        var modules = new IntPtr[1024]; int bytes;
                        if (!EnumProcessModulesEx(l.Process, modules, modules.Length * IntPtr.Size, out bytes, 3)) continue;
                        for (int m = 0; m < Math.Min(modules.Length, bytes / IntPtr.Size); m++) {
                            var name = new StringBuilder(260); GetModuleBaseNameW(l.Process, modules[m], name, 260);
                            if (String.Equals(name.ToString(), s.ModuleName, StringComparison.OrdinalIgnoreCase)) { moduleBase = modules[m].ToInt64(); break; }
                        }
                        if (moduleBase == 0) continue;
                    }
                    long checkAddr = moduleBase + s.CheckAddr, patchAddr = moduleBase + s.PatchAddr;
                    var cur = new byte[s.Expect.Length]; IntPtr rd;
                    if (!ReadProcessMemory(l.Process, new IntPtr(checkAddr), cur, new IntPtr(cur.Length), out rd) || rd.ToInt64() != cur.Length) continue;
                    bool match = true; for (int i = 0; i < cur.Length; i++) if (cur[i] != s.Expect[i]) { match = false; break; }
                    if (!match) continue;
                    uint old; IntPtr wr;
                    if (!VirtualProtectEx(l.Process, new IntPtr(patchAddr), new IntPtr(s.Patch.Length), 0x40, out old)) { s.Error = "VirtualProtectEx " + Marshal.GetLastWin32Error(); continue; }
                    bool ok = WriteProcessMemory(l.Process, new IntPtr(patchAddr), s.Patch, new IntPtr(s.Patch.Length), out wr);
                    uint dummy; VirtualProtectEx(l.Process, new IntPtr(patchAddr), new IntPtr(s.Patch.Length), old, out dummy);
                    FlushInstructionCache(l.Process, new IntPtr(patchAddr), new IntPtr(s.Patch.Length));
                    var chk = new byte[s.Patch.Length];
                    ReadProcessMemory(l.Process, new IntPtr(patchAddr), chk, new IntPtr(chk.Length), out rd);
                    bool same = true; for (int i = 0; i < chk.Length; i++) if (chk[i] != s.Patch[i]) same = false;
                    if (ok && same) { s.Done = true; s.AppliedMs = sw.Elapsed.TotalMilliseconds; } else s.Error = "write failed " + Marshal.GetLastWin32Error();
                }
                if (all) { PatcherState = "applied"; return; }
                if (WaitForSingleObject(l.Event, 0) == 0) {
                    bool done = true; foreach (var s in specs) if (s.BeforeTicket && !s.Done) done = false;
                    if (!done) { TerminateProcess(l.Process, 0xDEAD); PatcherKilledAtMs = sw.Elapsed.TotalMilliseconds; PatcherState = "killed: ticket read before patches applied"; return; }
                }
                uint code; GetExitCodeProcess(l.Process, out code);
                if (code != 259) { PatcherState = "client exited before patches applied"; return; }
                Thread.Sleep(0);
            }
            PatcherState = "timeout";
        }) { IsBackground = true, Priority = ThreadPriority.Highest };
        t.Start();
    }


    public static void StopOwned(Launched launched) {
        uint code;
        if (GetExitCodeProcess(launched.Process, out code) && code == 259) TerminateProcess(launched.Process, 0xDEAD);
    }
    public static void Release(Launched launched) {
        foreach (var handle in new[] { launched.Thread, launched.Process, launched.Map, launched.Event })
            if (handle != IntPtr.Zero) CloseHandle(handle);
    }}
