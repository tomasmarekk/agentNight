# One night on a Windows runner, with a real power-off at the end:
# a stand-in Claude Code session works, the switch is turned on, the session finishes
# and agentNight shuts the machine down.
param([Parameter(Mandatory)][ValidateSet("start", "arm", "finish")][string]$Step)
$ErrorActionPreference = "Stop"

if ($env:GITHUB_ACTIONS -ne "true") {
  throw "This script powers the machine off. It only runs on GitHub Actions runners."
}

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System; using System.Runtime.InteropServices;
public class Win {
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(int f, int x, int y, int d, int e);
}
"@
[Win]::SetProcessDPIAware() | Out-Null

$night = Join-Path $env:RUNNER_TEMP "night"
$shots = Join-Path $env:RUNNER_TEMP "screenshots"
# The switch, in window client coordinates.
$switchX = 291; $switchY = 81

function Set-Status([string]$status) {
  $registry = @{
    pid = [int]$env:AGENT_PID; sessionId = "smoke"; cwd = "$night\demo"
    startedAt = [long]$env:AGENT_STARTED; status = $status
    statusUpdatedAt = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    name = "Overnight refactor"; entrypoint = "cli"
  } | ConvertTo-Json -Compress
  [IO.File]::WriteAllText("$night\.claude\sessions\$($env:AGENT_PID).json", $registry)
}

function Get-AppWindow {
  for ($i = 0; $i -lt 60; $i++) {
    $p = Get-Process agent-night -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowTitle -eq "agentNight" }
    if ($p) { return $p.MainWindowHandle }
    Start-Sleep 1
  }
  throw "The agentNight window did not appear"
}

function Get-ClientPoint([IntPtr]$hwnd, [int]$x, [int]$y) {
  $p = New-Object Win+POINT
  $p.X = $x; $p.Y = $y
  [Win]::ClientToScreen($hwnd, [ref]$p) | Out-Null
  return $p
}

function Save-Shot([string]$name) {
  $bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
  $bmp = New-Object System.Drawing.Bitmap $bounds.Width, $bounds.Height
  [System.Drawing.Graphics]::FromImage($bmp).CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
  $bmp.Save("$shots\$name.png", [System.Drawing.Imaging.ImageFormat]::Png)
  return $bmp
}
Add-Type -AssemblyName System.Windows.Forms

switch ($Step) {
  "start" {
    New-Item -ItemType Directory -Force "$night\.claude\sessions", $shots | Out-Null
    # Any long-running program named claude.exe looks like a Claude Code process.
    Copy-Item "$env:SystemRoot\System32\PING.EXE" "$night\claude.exe"
    $agent = Start-Process "$night\claude.exe" -ArgumentList "-n", "3600", "127.0.0.1" -WindowStyle Hidden -PassThru
    $env:AGENT_PID = $agent.Id
    $env:AGENT_STARTED = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    "AGENT_PID=$env:AGENT_PID" >> $env:GITHUB_ENV
    "AGENT_STARTED=$env:AGENT_STARTED" >> $env:GITHUB_ENV
    Set-Status "busy"

    $env:AGENTNIGHT_HOME = $night
    $env:AGENTNIGHT_GRACE_SECS = "10"
    $env:AGENTNIGHT_ONLINE_GRACE_SECS = "5"
    $env:AGENTNIGHT_COUNTDOWN_SECS = "10"
    Start-Process "$env:GITHUB_WORKSPACE\src-tauri\target\release\agent-night.exe"
    Get-AppWindow | Out-Null
    Start-Sleep 8
    Save-Shot "1-working" | Out-Null
    & "$env:GITHUB_WORKSPACE\src-tauri\target\release\agent-night.exe" --scan | Out-String | Tee-Object "$shots\scan.json"
    if (-not (Select-String -Path "$shots\scan.json" -Pattern '"state": "working"' -Quiet)) {
      throw "agentNight does not see the stand-in session as working"
    }
  }
  "arm" {
    $hwnd = Get-AppWindow
    [Win]::SetForegroundWindow($hwnd) | Out-Null
    $p = Get-ClientPoint $hwnd $switchX $switchY
    [Win]::SetCursorPos($p.X, $p.Y) | Out-Null
    Start-Sleep -Milliseconds 200
    [Win]::mouse_event(2, 0, 0, 0, 0)
    Start-Sleep -Milliseconds 80
    [Win]::mouse_event(4, 0, 0, 0, 0)
    Start-Sleep 3
    $bmp = Save-Shot "2-armed"
    # Left part of the switch track turns accent blue when it is on.
    $track = Get-ClientPoint $hwnd ($switchX - 15) $switchY
    $c = $bmp.GetPixel($track.X, $track.Y)
    Write-Output "switch track color: $($c.R),$($c.G),$($c.B)"
    if ($c.B -lt 180 -or $c.R -gt 170) { throw "The switch did not turn on" }
  }
  "finish" {
    Set-Status "idle"
    Write-Output "Agent finished at $(Get-Date -Format o). agentNight should power this machine off in about 30 seconds."
    Start-Sleep 300
  }
}
