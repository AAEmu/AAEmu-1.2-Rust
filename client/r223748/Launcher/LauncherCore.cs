using System;
using System.IO;
using System.Net;
using System.Net.Sockets;
using System.Security.Cryptography;
using System.Text;
using System.Xml.Serialization;
using System.Collections.Generic;
using System.Text.RegularExpressions;
using System.Runtime.InteropServices;

namespace AAEmu.Launcher {
    public sealed class Settings {
        public string Server = "127.0.0.1";
        public int Port = 1237;
        public string Username = "";
        public string ClientPath = "";
        public bool RememberPassword = true;
        public string PasswordCipher = "";
        public int WindowMode;
        public string Resolution = "Use game setting";
        public int VSync;
        public bool MuteSound;
        public bool MuteMusic;
        public bool Minimized;
    }

    public sealed class SettingsStore {
        public readonly string DirectoryPath;
        public string FilePath { get { return Path.Combine(DirectoryPath, "settings.xml"); } }
        static readonly byte[] Entropy = Encoding.UTF8.GetBytes("AAEmu-r223748-launcher-v1");
        public SettingsStore(string directory) { DirectoryPath = directory; }
        public Settings Load() {
            if (!File.Exists(FilePath)) return new Settings();
            using (var input = File.OpenRead(FilePath)) return (Settings)new XmlSerializer(typeof(Settings)).Deserialize(input);
        }
        public void Save(Settings value, string password) {
            value.PasswordCipher = value.RememberPassword && !String.IsNullOrEmpty(password) ? Protect(password) : "";
            Directory.CreateDirectory(DirectoryPath);
            string temporary = FilePath + ".tmp";
            using (var output = new FileStream(temporary, FileMode.Create, FileAccess.Write, FileShare.None)) {
                new XmlSerializer(typeof(Settings)).Serialize(output, value);
                output.Flush(true);
            }
            if (File.Exists(FilePath)) File.Replace(temporary, FilePath, FilePath + ".bak");
            else File.Move(temporary, FilePath);
            // A removed password must not remain in the previous settings generation.
            if (File.Exists(FilePath + ".bak")) File.Delete(FilePath + ".bak");
        }
        public static string Protect(string value) {
            byte[] bytes = Encoding.UTF8.GetBytes(value);
            try { return Convert.ToBase64String(ProtectedData.Protect(bytes, Entropy, DataProtectionScope.CurrentUser)); }
            finally { Array.Clear(bytes, 0, bytes.Length); }
        }
        public static string Unprotect(string value) {
            if (String.IsNullOrEmpty(value)) return "";
            byte[] bytes = ProtectedData.Unprotect(Convert.FromBase64String(value), Entropy, DataProtectionScope.CurrentUser);
            try { return Encoding.UTF8.GetString(bytes); }
            finally { Array.Clear(bytes, 0, bytes.Length); }
        }
    }

    public static class LaunchOptions {
        public static readonly string[] Resolutions = { "Use game setting", "1280 x 768", "1600 x 900", "1920 x 1080", "2560 x 1440" };
        [StructLayout(LayoutKind.Explicit, Size=156)] struct DisplayMode {
            [FieldOffset(36)] public ushort Size;
            [FieldOffset(40)] public uint Fields;
            [FieldOffset(104)] public uint Bits;
            [FieldOffset(108)] public uint Width;
            [FieldOffset(112)] public uint Height;
        }
        [DllImport("user32", CharSet=CharSet.Ansi)] static extern int ChangeDisplaySettingsExA(string device,ref DisplayMode mode,IntPtr hwnd,uint flags,IntPtr parameter);
        public static bool SupportsResolution(string value) {
            if(value==Resolutions[0])return true;
            if(Array.IndexOf(Resolutions,value)<0)return false;
            var parts=value.Split('x');
            var mode=new DisplayMode {Size=156,Fields=0x1c0000,Bits=32,Width=UInt32.Parse(parts[0].Trim()),Height=UInt32.Parse(parts[1].Trim())};
            // CDS_TEST | CDS_FULLSCREEN: query only, matching the client's own test.
            return ChangeDisplaySettingsExA(null,ref mode,IntPtr.Zero,6,IntPtr.Zero)==0;
        }
        public static string[] AvailableResolutions() {return Array.FindAll(Resolutions,SupportsResolution);}
        public static void Validate(Settings s, string password) {
            IPAddress ip;
            if (!IPAddress.TryParse(s.Server, out ip) || ip.AddressFamily != AddressFamily.InterNetwork) throw new Exception("Enter a valid IPv4 server address.");
            if (s.Port < 1 || s.Port > 65535) throw new Exception("Login port must be between 1 and 65535.");
            if (!Regex.IsMatch(s.Username, @"\A[a-zA-Z0-9_]{3,32}\z")) throw new Exception("Username: 3-32 letters, digits or underscores.");
            if (String.IsNullOrEmpty(password)) throw new Exception("Enter your account password.");
            if (s.WindowMode < 0 || s.WindowMode > 2 || s.VSync < 0 || s.VSync > 2 || Array.IndexOf(Resolutions, s.Resolution) < 0) throw new Exception("Invalid launch options.");
        }
        public static Dictionary<string,string> Cvars(Settings s) {
            var result = new Dictionary<string,string>(StringComparer.OrdinalIgnoreCase);
            if (s.WindowMode != 0) result["r_Fullscreen"] = s.WindowMode == 1 ? "0" : "1";
            if (s.Resolution != Resolutions[0]) {
                if (Array.IndexOf(Resolutions, s.Resolution) < 0) throw new Exception("Unsupported resolution.");
                var parts = s.Resolution.Split('x');
                result["r_Width"] = parts[0].Trim(); result["r_Height"] = parts[1].Trim();
            }
            if (s.VSync != 0) result["r_VSync"] = s.VSync == 1 ? "1" : "0";
            // Explicitly restore audio too, so a previously muted launch is reversible.
            result["s_SoundEnable"] = s.MuteSound ? "0" : "1";
            result["s_MusicEnable"] = s.MuteMusic ? "0" : "1";
            return result;
        }
        public static string Arguments(Settings s) {
            var b = new StringBuilder("-t +auth_ip " + s.Server + " -auth_port " + s.Port + " -handle {HANDLE} -lang en_us");
            foreach (var item in Cvars(s)) b.Append(" +").Append(item.Key).Append(' ').Append(item.Value);
            return b.Append(" +acpxmk").ToString();
        }
        public static void VerifyClient(string exe) {
            var hashes = new Dictionary<string,string> {
                {"archeage.exe", "1DC7256E3C13A1C6D2F232BB67921ABDD9823AA29D5749CDE526F2146A45D352"},
                {"x2game.dll", "9192135D0C47F69369927B198C92C1E212397C2D036E59F076CD25F81759DC7A"},
                {"CryNetwork.dll", "2C70A725F90FE4C59A76DA551EC39BE308C559040355BE9D299091568E8463D4"}
            };
            string bin = Path.GetDirectoryName(Path.GetFullPath(exe));
            if (!String.Equals(Path.GetFileName(exe), "archeage.exe", StringComparison.OrdinalIgnoreCase)) throw new Exception("Choose bin32\\archeage.exe from your r223748 client.");
            foreach (var item in hashes) {
                string path = Path.Combine(bin, item.Key);
                if (!File.Exists(path)) throw new Exception("Missing client file: " + item.Key);
                using (var sha = SHA256.Create()) using (var file = File.OpenRead(path)) {
                    if (BitConverter.ToString(sha.ComputeHash(file)).Replace("-", "") != item.Value) throw new Exception("Wrong or modified client: " + item.Key + ". Use the stock r223748 client.");
                }
            }
            var pak = new FileInfo(Path.Combine(Path.GetDirectoryName(bin), "game_pak"));
            if (!pak.Exists || pak.Length != 24720175616L) throw new Exception("The r223748 game_pak is missing or has the wrong size.");
        }
        // ArcheAge restores renderer preferences after parsing its command line.
        // Update only explicitly selected display keys; preserve every unrelated line.
        public static void ApplyDisplayPreferences(Settings settings, string file) {
            var selected = Cvars(settings);
            selected.Remove("s_SoundEnable"); selected.Remove("s_MusicEnable");
            if (selected.Count == 0) return;
            if (File.Exists(file + ".aaemu.tmp")) throw new Exception("A previous settings update is incomplete. Close other launchers first.");
            var lines = new List<string>();
            if (File.Exists(file)) lines.AddRange(File.ReadAllLines(file));
            var seen = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
            for (int i = 0; i < lines.Count; i++) {
                var m = Regex.Match(lines[i], @"^\s*([a-zA-Z0-9_]+)\s*=");
                string value;
                if (m.Success && selected.TryGetValue(m.Groups[1].Value, out value)) {
                    lines[i] = m.Groups[1].Value + " = " + value; seen.Add(m.Groups[1].Value);
                }
            }
            foreach (var item in selected) if (!seen.Contains(item.Key)) lines.Add(item.Key + " = " + item.Value);
            Directory.CreateDirectory(Path.GetDirectoryName(file));
            File.WriteAllLines(file + ".aaemu.tmp", lines, new UTF8Encoding(false));
            if (File.Exists(file)) File.Replace(file + ".aaemu.tmp", file, file + ".aaemu.bak");
            else File.Move(file + ".aaemu.tmp", file);
        }
        static ClientRuntime.PatchSpec Patch(string name, long check, string expected, long address, string replacement) {
            return new ClientRuntime.PatchSpec { Name=name, CheckAddr=check, PatchAddr=address, Expect=Hex(expected), Patch=Hex(replacement) };
        }
        static byte[] Hex(string value) { return Array.ConvertAll(value.Split(' '), n => Convert.ToByte(n,16)); }
        public static ClientRuntime.PatchSpec[] Patches() {
            var network = Patch("aaemu-no-crypto",0x8FCB7,"6A 01 E8 85 C3 FF FF 8B C8 E8 6B 0D 01 00",0x8FCB7,"E9 90 01 00 00 90 90");
            network.ModuleName="CryNetwork.dll"; network.BeforeTicket=false;
            return new[] {
                Patch("hs-gate",0x390A9D41,"0F B6 45 BB 85 C0 74 7C",0x390A9D47,"EB"),
                Patch("no-browser",0x39185000,"6A 00 6A 00 8B C6 50 68 DC 15 9C 39 6A 00 FF 15 3C 69 FF 38",0x3918500E,"83 C4 18 90 90 90"), network
            };
        }
    }
}
