param(
    [string]$Server,
    [int]$LoginPort = 0,
    [string]$ClientPath,
    [Management.Automation.PSCredential]$Credential,
    [switch]$NoGui,
    [switch]$ValidateOnly,
    [switch]$Minimized
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
if (-not $ClientPath) { $ClientPath = Join-Path $root 'bin32\archeage.exe' }
$ClientPath = [IO.Path]::GetFullPath($ClientPath)
$settings = Get-Content -LiteralPath (Join-Path $root 'server.json') -Raw | ConvertFrom-Json
if (-not $Server) { $Server = [string]$settings.Server }
if ($LoginPort -eq 0) { $LoginPort = [int]$settings.LoginPort }
function Test-Client {
    $bin = Split-Path -Parent $ClientPath
    $hashes = @{
        'archeage.exe' = '1DC7256E3C13A1C6D2F232BB67921ABDD9823AA29D5749CDE526F2146A45D352'
        'x2game.dll' = '9192135D0C47F69369927B198C92C1E212397C2D036E59F076CD25F81759DC7A'
        'CryNetwork.dll' = '2C70A725F90FE4C59A76DA551EC39BE308C559040355BE9D299091568E8463D4'
    }
    foreach ($name in $hashes.Keys) {
        $file = Join-Path $bin $name
        if (-not (Test-Path -LiteralPath $file) -or
            (Get-FileHash -Algorithm SHA256 -LiteralPath $file).Hash -ne $hashes[$name]) {
            throw "Wrong or modified client file: $name. This launcher requires r223748."
        }
    }
    $pak = Join-Path (Split-Path -Parent $bin) 'game_pak'
    if (-not (Test-Path -LiteralPath $pak) -or (Get-Item -LiteralPath $pak).Length -ne 24720175616) {
        throw 'The r223748 game_pak is missing or has the wrong size.'
    }
}
Test-Client
if ($ValidateOnly) { 'PASS r223748 executable hashes and game_pak size'; return }
Add-Type -Path (Join-Path $PSScriptRoot 'ClientRuntime.cs')
$dx = Join-Path $PSScriptRoot 'DirectX'
if (Test-Path -LiteralPath $dx) { $env:PATH = "$dx;$env:PATH" }
function New-Patch($Name, [long]$Check, $Expected, [long]$Address, $Replacement) {
    $p = New-Object ClientRuntime+PatchSpec
    $p.Name = $Name; $p.CheckAddr = $Check; $p.PatchAddr = $Address
    $p.Expect = [byte[]]($Expected -split ' ' | ForEach-Object { [Convert]::ToByte($_,16) })
    $p.Patch = [byte[]]($Replacement -split ' ' | ForEach-Object { [Convert]::ToByte($_,16) })
    $p
}
function Start-Game([string]$Address, [int]$Port, [Management.Automation.PSCredential]$Login) {
    $ip = $null
    if (-not [Net.IPAddress]::TryParse($Address, [ref]$ip) -or
        $ip.AddressFamily -ne [Net.Sockets.AddressFamily]::InterNetwork -or
        $Port -lt 1 -or $Port -gt 65535) { throw 'Enter a valid IPv4 address and login port.' }
    if ($Login.UserName -notmatch '^[a-zA-Z0-9_]{3,32}$') { throw 'Username: 3-32 letters, digits or underscores.' }
    if (@(Get-Process archeage -ErrorAction SilentlyContinue).Count) { throw 'Close the running ArcheAge client first.' }
    $connection = New-Object Net.Sockets.TcpClient
    try {
        $task = $connection.ConnectAsync($Address, $Port)
        if (-not $task.Wait(5000)) { throw 'Login server did not respond within five seconds.' }
        $task.GetAwaiter().GetResult()
    } finally { $connection.Dispose() }
    $patches = @(
        (New-Patch 'hs-gate' 0x390A9D41 '0F B6 45 BB 85 C0 74 7C' 0x390A9D47 'EB'),
        (New-Patch 'no-browser' 0x39185000 '6A 00 6A 00 8B C6 50 68 DC 15 9C 39 6A 00 FF 15 3C 69 FF 38' 0x3918500E '83 C4 18 90 90 90')
    )
    $network = New-Patch 'aaemu-no-crypto' 0x8FCB7 '6A 01 E8 85 C3 FF FF 8B C8 E8 6B 0D 01 00' 0x8FCB7 'E9 90 01 00 00 90 90'
    $network.ModuleName = 'CryNetwork.dll'; $network.BeforeTicket = $false
    $patches += $network
    $arguments = "-t +auth_ip $Address -auth_port $Port -handle {HANDLE} -lang en_us -nosplash +r_Fullscreen 0 +r_Width 1280 +r_Height 720 +acpxmk"
    $password = $Login.GetNetworkCredential().Password
    try { $launched = [ClientRuntime]::Launch($ClientPath, $arguments, $Login.UserName, $password, $(if ($Minimized) { 7 } else { 1 })) }
    finally { $password = $null }
    [ClientRuntime]::StartPatcher($launched, [ClientRuntime+PatchSpec[]]$patches, 60000)
    try {
        while ([ClientRuntime]::ExitState($launched) -eq 'running') {
            $state = [ClientRuntime]::PatcherState
            if ($state -eq 'applied') {
                "Client PID $($launched.Pid): runtime patches applied; server ${Address}:$Port"
                return $launched.Pid
            }
            if ($state -eq 'timeout' -or $state.StartsWith('killed:') -or $state.StartsWith('client exited')) {
                throw "Client launch failed: $state"
            }
            [ClientRuntime]::HandleDialogs($launched.Pid) | Out-Null
            if (-not $NoGui) { [Windows.Forms.Application]::DoEvents() }
            Start-Sleep -Milliseconds 10
        }
        throw "Client $([ClientRuntime]::ExitState($launched)) before patches completed."
    } catch {
        # Only the process created by this invocation is eligible for cleanup.
        [ClientRuntime]::StopOwned($launched)
        throw
    } finally { [ClientRuntime]::Release($launched) }
}
if ($NoGui) {
    if (-not $Credential) { $Credential = Get-Credential -UserName $settings.Username -Message 'AAEmu account' }
    Start-Game $Server $LoginPort $Credential
    return
}
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
[Windows.Forms.Application]::EnableVisualStyles()
$form = New-Object Windows.Forms.Form
$form.Text = 'AAEmu - ArcheAge 1.2 r223748'
$form.ClientSize = New-Object Drawing.Size(420,280)
$form.FormBorderStyle = 'FixedDialog'; $form.MaximizeBox = $false; $form.StartPosition = 'CenterScreen'
$boxes = @{}
$labels = @('Server IP','Login port','Username','Password')
$values = @($Server,[string]$LoginPort,[string]$settings.Username,'')
for ($i = 0; $i -lt 4; $i++) {
    $label = New-Object Windows.Forms.Label
    $label.Text = $labels[$i]; $label.Location = New-Object Drawing.Point(20,(25+40*$i)); $label.AutoSize = $true
    $box = New-Object Windows.Forms.TextBox
    $box.Location = New-Object Drawing.Point(120,(20+40*$i)); $box.Width = 275; $box.Text = $values[$i]
    if ($i -eq 3) { $box.UseSystemPasswordChar = $true }
    $boxes[$labels[$i]] = $box; $form.Controls.Add($label); $form.Controls.Add($box)
}
$status = New-Object Windows.Forms.Label
$status.Location = New-Object Drawing.Point(20,220); $status.Size = New-Object Drawing.Size(380,50)
$status.Text = 'Enter your account password, then click Play.'
$play = New-Object Windows.Forms.Button
$play.Text = 'Play'; $play.Location = New-Object Drawing.Point(290,180); $play.Width = 105
$form.Controls.Add($play); $form.Controls.Add($status); $form.AcceptButton = $play
$script:launching = $false
$form.Add_FormClosing({ param($sender,$event) if ($script:launching) { $event.Cancel = $true } })
$play.Add_Click({
    $script:launching = $true; $play.Enabled = $false; $status.Text = 'Connecting and starting client...'
    try {
        $port = 0
        if (-not [int]::TryParse($boxes['Login port'].Text,[ref]$port)) { throw 'Login port must be a number.' }
        if (-not $boxes.Password.Text) { throw 'Enter an account password.' }
        $secret = ConvertTo-SecureString $boxes.Password.Text -AsPlainText -Force
        $login = New-Object Management.Automation.PSCredential($boxes.Username.Text, $secret)
        Start-Game $boxes['Server IP'].Text.Trim() $port $login | Out-Null
        $boxes.Password.Clear()
        $status.Text = 'Client launched. You may close this launcher.'
    } catch { $status.Text = $_.Exception.Message }
    finally { $script:launching = $false; $play.Enabled = $true }
})
[void]$form.ShowDialog()
