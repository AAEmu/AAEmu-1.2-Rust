using System;
using System.Diagnostics;
using System.Drawing;
using System.Drawing.Drawing2D;
using System.IO;
using System.Net.Sockets;
using System.Reflection;
using System.Security.Cryptography;
using System.Threading;
using System.Threading.Tasks;
using System.Windows.Forms;

namespace AAEmu.Launcher {
    static class Program {
        [STAThread]
        static int Main(string[] args) {
            try {
                if (args.Length > 0 && args[0] == "--self-test") { SelfTests.Run(args.Length > 1 ? args[1] : null); return 0; }
                Application.EnableVisualStyles(); Application.SetCompatibleTextRenderingDefault(false);
                bool smoke = args.Length == 2 && args[0] == "--smoke-ui";
                bool created;
                using(var gate=new Mutex(true,"Local\\AAEmu.Launcher.r223748",out created)) {
                    if(!created) {MessageBox.Show("AAEmu Launcher is already open.","AAEmu");return 0;}
                    Application.Run(new LauncherForm(smoke ? args[1] : null));
                }
                return 0;
            } catch (Exception e) {
                if(args.Length>0&&args[0]=="--self-test") {if(args.Length>1) File.WriteAllText(args[1],"FAIL "+e.Message);return 1;}
                MessageBox.Show(e.Message,"AAEmu launcher",MessageBoxButtons.OK,MessageBoxIcon.Error); return 1;
            }
        }
    }
    sealed class Header : Panel {
        readonly Image art;
        public Header() {
            DoubleBuffered=true;
            using (var input=Assembly.GetExecutingAssembly().GetManifestResourceStream("AAEmu.Banner")) using(var source=Image.FromStream(input)) art=new Bitmap(source);
        }
        protected override void OnPaint(PaintEventArgs e) {
            base.OnPaint(e);
            float scale=Width/780f;
            e.Graphics.InterpolationMode=InterpolationMode.HighQualityBicubic;
            // Preserve the artwork's aspect ratio; use the composed central banner strip.
            float h=art.Width*(Height/(float)Width);
            e.Graphics.DrawImage(art,ClientRectangle,new RectangleF(0,art.Height*0.42f-h/2,art.Width,h),GraphicsUnit.Pixel);
            using(var shade=new LinearGradientBrush(ClientRectangle,Color.FromArgb(220,10,22,30),Color.FromArgb(0,10,22,30),0f)) e.Graphics.FillRectangle(shade,ClientRectangle);
            using(var title=new Font("Segoe UI",32*scale,FontStyle.Bold)) using(var sub=new Font("Segoe UI",10*scale)) using(var muted=new SolidBrush(Color.FromArgb(203,190,159))) {
                e.Graphics.DrawString("AAEmu",title,Brushes.White,24*scale,40*scale);
                e.Graphics.DrawString("ArcheAge 1.2.5.0",sub,muted,28*scale,101*scale);
            }
            using(var gold=new Pen(Color.FromArgb(186,153,89),2)) e.Graphics.DrawLine(gold,0,Height-1,Width,Height-1);
        }
        protected override void Dispose(bool disposing) { if(disposing) art.Dispose(); base.Dispose(disposing); }
    }
    sealed class LauncherForm : Form {
        readonly Color panel=Color.FromArgb(24,40,49), ink=Color.FromArgb(230,236,237), dim=Color.FromArgb(151,172,181);
        readonly SettingsStore store;
        Settings settings;
        readonly TextBox server=new TextBox(), port=new TextBox(), username=new TextBox(), password=new TextBox(), client=new TextBox();
        readonly ComboBox mode=new ComboBox(), resolution=new ComboBox(), vsync=new ComboBox();
        readonly CheckBox remember=new CheckBox(), muteSound=new CheckBox(), muteMusic=new CheckBox(), minimized=new CheckBox();
        readonly Label status=new Label();
        readonly Button play=new Button();
        readonly System.Windows.Forms.Timer saveTimer=new System.Windows.Forms.Timer();
        bool loading=true, launching;
        public LauncherForm(string smokePath) {
            Text="AAEmu Launcher"; ClientSize=new Size(780,640); StartPosition=FormStartPosition.CenterScreen;
            FormBorderStyle=FormBorderStyle.FixedSingle; MaximizeBox=false; BackColor=Color.FromArgb(13,26,34); ForeColor=ink;
            Font=new Font("Segoe UI",10); AutoScaleMode=AutoScaleMode.Dpi;
            Icon=Icon.ExtractAssociatedIcon(Application.ExecutablePath);
            store=new SettingsStore(Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),"AAEmu","Launcher","r223748"));
            string warning="";
            try { settings=smokePath==null ? store.Load() : new Settings(); }
            catch { settings=new Settings(); warning="Saved settings could not be read. Choose your client and sign in again."; }
            if(String.IsNullOrEmpty(settings.ClientPath)) settings.ClientPath=DiscoverClient();
            Controls.Add(new Header {Location=new Point(0,0),Size=new Size(780,156)});
            Caption("CONNECTION",26,166);
            Field("Server IP",server,26,208,456); Field("Login port",port,510,208,244);
            Field("Username",username,26,272,342); Field("Password",password,396,272,358);
            password.UseSystemPasswordChar=true;
            remember.Text="Remember password"; remember.SetBounds(26,309,218,26); remember.ForeColor=dim; remember.AutoSize=false; Controls.Add(remember);
            var protection=new Label {Text="Protected by your Windows account",ForeColor=dim,Location=new Point(396,313),AutoSize=true,Font=new Font("Segoe UI",9)};Controls.Add(protection);
            Field("Client executable",client,26,372,612);
            var browse=Button("Browse",650,370,104,30);
            browse.Click+=delegate { using(var dialog=new OpenFileDialog {Title="Select r223748 bin32/archeage.exe",Filter="ArcheAge client|archeage.exe",CheckFileExists=true}) { if(dialog.ShowDialog(this)==DialogResult.OK) client.Text=dialog.FileName; } };
            Caption("LAUNCH OPTIONS",26,404);
            Choice("Display mode",mode,new[]{"Use game setting","Windowed","Fullscreen"},26,452,220);
            Choice("Resolution",resolution,LaunchOptions.AvailableResolutions(),266,452,220);
            Choice("VSync",vsync,new[]{"Use game setting","On","Off"},506,452,248);
            Check(muteSound,"Mute all sound",26,497); Check(muteMusic,"Mute music",266,497); Check(minimized,"Start minimized",506,497);
            var rule=new Panel {BackColor=Color.FromArgb(39,60,68),Location=new Point(26,538),Size=new Size(728,1)};Controls.Add(rule);
            status.SetBounds(26,551,728,28); status.ForeColor=dim;status.Font=new Font("Segoe UI",9);Controls.Add(status);
            var version=new Label {Text="ArcheAge 1.2.5.0  /  r223748\nSettings save automatically",ForeColor=dim,Location=new Point(26,590),Size=new Size(440,42),Font=new Font("Segoe UI",9)};Controls.Add(version);
            play.Text="PLAY";play.SetBounds(534,582,220,42);play.FlatStyle=FlatStyle.Flat;play.FlatAppearance.BorderSize=0;play.BackColor=Color.FromArgb(80,182,160);play.ForeColor=Color.FromArgb(9,29,32);play.Font=new Font("Segoe UI",12,FontStyle.Bold);Controls.Add(play);AcceptButton=play;
            server.Text=settings.Server;port.Text=settings.Port.ToString();username.Text=settings.Username;client.Text=settings.ClientPath;remember.Checked=settings.RememberPassword;
            mode.SelectedIndex=Math.Max(0,Math.Min(2,settings.WindowMode));resolution.SelectedIndex=Math.Max(0,resolution.Items.IndexOf(settings.Resolution));vsync.SelectedIndex=Math.Max(0,Math.Min(2,settings.VSync));
            muteSound.Checked=settings.MuteSound;muteMusic.Checked=settings.MuteMusic;minimized.Checked=settings.Minimized;
            try { password.Text=SettingsStore.Unprotect(settings.PasswordCipher); } catch { settings.PasswordCipher="";warning="Re-enter your password; the saved password belongs to a different Windows profile or is damaged."; }
            status.Text=warning.Length>0 ? warning : String.IsNullOrEmpty(client.Text) ? "Choose your client executable to get started." : "Ready to connect.";
            saveTimer.Interval=600;saveTimer.Tick+=delegate {saveTimer.Stop();Save();};
            foreach(var box in new[]{server,port,username,password,client}) box.TextChanged+=Changed;
            foreach(var combo in new[]{mode,resolution,vsync}) combo.SelectedIndexChanged+=Changed;
            foreach(var box in new[]{remember,muteSound,muteMusic,minimized}) box.CheckedChanged+=Changed;
            play.Click+=async delegate { await Play(); };
            FormClosing+=delegate(object sender,FormClosingEventArgs e) { if(launching) {e.Cancel=true;status.Text="Please wait for client startup to finish.";} else {saveTimer.Stop(); if(smokePath==null) Save();} };
            FormClosed+=delegate { saveTimer.Dispose(); };
            loading=false;
            if(smokePath!=null) {
                loading=true;server.Text="192.0.2.10";username.Text="Player";client.Text=@"C:\ArcheAge\bin32\archeage.exe";password.Text="preview";play.Enabled=false;
                Shown+=delegate {var timer=new System.Windows.Forms.Timer {Interval=2000};timer.Tick+=delegate {timer.Stop();using(var image=new Bitmap(ClientSize.Width,ClientSize.Height))using(var graphics=Graphics.FromImage(image)){graphics.CopyFromScreen(PointToScreen(Point.Empty),Point.Empty,ClientSize);image.Save(smokePath,System.Drawing.Imaging.ImageFormat.Png);}timer.Dispose();Close();};timer.Start();};
            }
        }
        static string DiscoverClient() {
            string dir=AppDomain.CurrentDomain.BaseDirectory;
            foreach(var root in new[]{dir,Path.GetDirectoryName(dir.TrimEnd(Path.DirectorySeparatorChar))}) {
                if(root==null) continue;
                string path=Path.Combine(root,"bin32","archeage.exe");if(File.Exists(path)) return path;
            }
            return "";
        }
        void Changed(object sender,EventArgs e) { if(loading)return; saveTimer.Stop();saveTimer.Start(); }
        Settings Read() {
            int number; if(!Int32.TryParse(port.Text,out number)) number=0;
            return new Settings {Server=server.Text.Trim(),Port=number,Username=username.Text.Trim(),ClientPath=client.Text.Trim(),RememberPassword=remember.Checked,WindowMode=mode.SelectedIndex,Resolution=resolution.Text,VSync=vsync.SelectedIndex,MuteSound=muteSound.Checked,MuteMusic=muteMusic.Checked,Minimized=minimized.Checked};
        }
        void Save() {
            if(loading)return;
            try {settings=Read();store.Save(settings,password.Text);}
            catch(Exception) {status.Text="Could not save settings. Check access to your Windows profile.";}
        }
        async Task Play() {
            if(launching)return; launching=true;play.Enabled=false;saveTimer.Stop();Save();
            string secret=password.Text;var selected=Read();
            try {
                LaunchOptions.Validate(selected,secret);
                if(!LaunchOptions.SupportsResolution(selected.Resolution)) throw new Exception("The display no longer supports this resolution. Choose Use game setting.");
                if(Process.GetProcessesByName("archeage").Length>0) throw new Exception("Close the running ArcheAge client first.");
                status.Text="Checking client and connecting...";
                await Task.Run(async delegate {
                    LaunchOptions.VerifyClient(selected.ClientPath);
                    using(var socket=new TcpClient()) {
                        var connect=socket.ConnectAsync(selected.Server,selected.Port);
                        if(await Task.WhenAny(connect,Task.Delay(5000))!=connect) throw new Exception("Login server did not respond within five seconds.");
                        await connect;
                    }
                    LaunchOptions.ApplyDisplayPreferences(selected,Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.MyDocuments),"ArcheAge","system.cfg"));
                    string dx=Path.Combine(AppDomain.CurrentDomain.BaseDirectory,"Launcher","DirectX");
                    if(Directory.Exists(dx)) Environment.SetEnvironmentVariable("PATH",dx+";"+Environment.GetEnvironmentVariable("PATH"));
                    var own=ClientRuntime.Launch(selected.ClientPath,LaunchOptions.Arguments(selected),selected.Username,secret,(short)(selected.Minimized?7:1));
                    try {
                        ClientRuntime.StartPatcher(own,LaunchOptions.Patches(),120000);
                        while(ClientRuntime.ExitState(own)=="running") {
                            string state=ClientRuntime.PatcherState;
                            if(state=="applied")return;
                            if(state=="timeout"||state.StartsWith("killed:")||state.StartsWith("client exited"))throw new Exception("Client startup failed: "+state);
                            ClientRuntime.HandleDialogs(own.Pid);await Task.Delay(10);
                        }
                        throw new Exception("Client exited before startup finished.");
                    } catch {ClientRuntime.StopOwned(own);throw;} finally {ClientRuntime.Release(own);}
                });
                status.Text="Client launched. You may close the launcher.";
            } catch(Exception e) {status.Text=e.Message;}
            finally {secret=null;launching=false;play.Enabled=true;}
        }
        void Caption(string text,int x,int y) {Controls.Add(new Label {Text=text,ForeColor=Color.FromArgb(208,181,130),Font=new Font("Segoe UI",9,FontStyle.Bold),Location=new Point(x,y),AutoSize=true});}
        void Field(string name,TextBox box,int x,int y,int width) {Controls.Add(new Label {Text=name,ForeColor=dim,Location=new Point(x,y-22),AutoSize=true});box.SetBounds(x,y,width,29);box.BackColor=panel;box.ForeColor=ink;box.BorderStyle=BorderStyle.FixedSingle;Controls.Add(box);}
        Button Button(string text,int x,int y,int width,int height) {var b=new Button {Text=text,BackColor=panel,ForeColor=ink,FlatStyle=FlatStyle.Flat};b.SetBounds(x,y,width,height);b.FlatAppearance.BorderColor=Color.FromArgb(54,81,93);Controls.Add(b);return b;}
        void Choice(string text,ComboBox box,string[] values,int x,int y,int width) {
            Controls.Add(new Label {Text=text,ForeColor=dim,Location=new Point(x,y-22),AutoSize=true});
            box.SetBounds(x,y,width,30);box.DropDownStyle=ComboBoxStyle.DropDownList;box.BackColor=panel;box.ForeColor=ink;box.FlatStyle=FlatStyle.Flat;
            box.DrawMode=DrawMode.OwnerDrawFixed;box.ItemHeight=23;box.Items.AddRange(values);
            box.DrawItem+=delegate(object sender,DrawItemEventArgs e) {
                int index=e.Index>=0?e.Index:box.SelectedIndex;
                using(var brush=new SolidBrush((e.State&DrawItemState.Selected)!=0?Color.FromArgb(36,75,78):panel))e.Graphics.FillRectangle(brush,e.Bounds);
                if(index>=0)TextRenderer.DrawText(e.Graphics,box.Items[index].ToString(),e.Font,e.Bounds,ink,TextFormatFlags.Left|TextFormatFlags.VerticalCenter);
            };
            Controls.Add(box);
        }
        void Check(CheckBox box,string text,int x,int y) {box.Text=text;box.SetBounds(x,y,235,28);box.ForeColor=ink;Controls.Add(box);}
    }
    static class SelfTests {
        static void Assert(bool value,string message) {if(!value)throw new Exception(message);}
        public static void Run(string report) {
            string root=Path.Combine(Path.GetTempPath(),"aaemu-launcher-test-"+Guid.NewGuid().ToString("N"));Directory.CreateDirectory(root);
            var lines=new System.Collections.Generic.List<string>();
            try {
                var store=new SettingsStore(root);var s=new Settings {Username="Tester",WindowMode=1,Resolution="1280 x 768",VSync=1};
                const string secret="Test-only-password-never-store-plaintext";store.Save(s,secret);
                var loaded=store.Load();Assert(SettingsStore.Unprotect(loaded.PasswordCipher)==secret,"Password round trip failed");
                Assert(!File.ReadAllText(store.FilePath).Contains(secret),"Plaintext password was saved");
                Assert(loaded.Username=="Tester"&&loaded.Port==1237&&loaded.Resolution==s.Resolution,"Settings were not restored");lines.Add("PASS DPAPI-encrypted password and settings round trip");
                s.RememberPassword=false;store.Save(s,secret);Assert(store.Load().PasswordCipher==""&&!File.Exists(store.FilePath+".bak"),"Forget password retained a previous cipher");lines.Add("PASS forgetting password removes saved cipher and previous generation");
                LaunchOptions.Validate(s,secret);string args=LaunchOptions.Arguments(s);Assert(args.Contains("+r_Width 1280")&&args.Contains("+r_Height 768")&&args.Contains("+r_Fullscreen 0")&&args.Contains("+r_VSync 1")&&!args.Contains(secret),"Incorrect launch arguments");
                s.Server="127.0.0.1 +exec bad.cfg";bool rejected=false;try{LaunchOptions.Validate(s,secret);}catch{rejected=true;}Assert(rejected,"Invalid IP accepted");s.Server="127.0.0.1";lines.Add("PASS allowlisted launch arguments and invalid IP rejection");
                string prefs=Path.Combine(root,"system.cfg");File.WriteAllText(prefs,"sys_spec_full = 4\nr_width = 1760\nr_height = 990\nr_vsync = 0\n");LaunchOptions.ApplyDisplayPreferences(s,prefs);
                string result=File.ReadAllText(prefs);Assert(result.Contains("sys_spec_full = 4")&&result.Contains("r_width = 1280")&&result.Contains("r_height = 768")&&File.Exists(prefs+".aaemu.bak"),"Display preferences not preserved");lines.Add("PASS explicit display preferences update with backup and unrelated settings preserved");
                Assert(LaunchOptions.Patches().Length==3,"Runtime patch set missing");lines.Add("PASS all three signature-checked runtime patches included");
                if(report!=null) File.WriteAllLines(report,lines);
            } finally {foreach(var name in new[]{"settings.xml","settings.xml.tmp","settings.xml.bak","system.cfg","system.cfg.aaemu.bak"}){string path=Path.Combine(root,name);if(File.Exists(path))File.Delete(path);}Directory.Delete(root);}
        }
    }
}
