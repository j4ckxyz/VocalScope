# Runs the published Windows app on a CI runner's desktop and saves
# screenshots of it into ./screenshots. Used by .github/workflows/build.yml.
#
# It fails if the app does not start, so it doubles as a launch test.
$ErrorActionPreference = 'Stop'

$exe = Resolve-Path 'publish/VocalScope/VocalScope.exe'
$out = New-Item -ItemType Directory -Force 'screenshots'
$env:VOCALSCOPE_DATA_ROOT = Join-Path $env:RUNNER_TEMP 'vocalscope-data'

try { Set-DisplayResolution -Width 1920 -Height 1080 -Force | Out-Null } catch { Write-Host "Display resolution left unchanged: $_" }

python -m pip install --quiet numpy lameenc
python scripts/make_test_audio.py
$audio = Resolve-Path 'test-audio/synthetic-vocal-4min.mp3'

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class Native {
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left, Top, Right, Bottom; }
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr window);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr window, out RECT rect);
    [DllImport("dwmapi.dll")] public static extern int DwmGetWindowAttribute(IntPtr window, int attribute, out RECT rect, int size);
}
"@
[Native]::SetProcessDPIAware() | Out-Null

function Save-Region($path, $left, $top, $width, $height) {
    $bitmap = New-Object System.Drawing.Bitmap $width, $height
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $graphics.CopyFromScreen($left, $top, 0, 0, $bitmap.Size)
    $bitmap.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $graphics.Dispose(); $bitmap.Dispose()
}

function Capture($name, $arguments) {
    $started = Get-Date
    $process = if ($arguments) { Start-Process -FilePath $exe -ArgumentList $arguments -PassThru } else { Start-Process -FilePath $exe -PassThru }
    Start-Sleep -Seconds 12
    $process.Refresh()
    if ($process.HasExited) {
        Write-Host "VocalScope exited early (code $($process.ExitCode)). Recent application errors:"
        Get-WinEvent -FilterHashtable @{ LogName = 'Application'; Level = 1, 2; StartTime = $started } -ErrorAction SilentlyContinue |
            Select-Object -First 4 | ForEach-Object { Write-Host $_.Message; Write-Host '---' }
        throw "VocalScope did not stay running for the '$name' screenshot."
    }
    $window = $process.MainWindowHandle
    [Native]::SetForegroundWindow($window) | Out-Null
    Start-Sleep -Seconds 2

    $rect = New-Object Native+RECT
    # The visible frame, without the invisible resize border.
    if ([Native]::DwmGetWindowAttribute($window, 9, [ref]$rect, 16) -ne 0) {
        [Native]::GetWindowRect($window, [ref]$rect) | Out-Null
    }
    $width = $rect.Right - $rect.Left
    $height = $rect.Bottom - $rect.Top
    Write-Host "$name: window $width x $height at $($rect.Left),$($rect.Top)"
    if ($width -le 0 -or $height -le 0) { throw "VocalScope has no visible window." }
    Save-Region (Join-Path $out "$name.png") $rect.Left $rect.Top $width $height

    Stop-Process -Id $process.Id -Force
    Start-Sleep -Seconds 2
}

try {
    Capture 'windows-main' "`"$audio`""
    Capture 'windows-start' $null
}
finally {
    $log = Join-Path $env:VOCALSCOPE_DATA_ROOT 'logs/vocalscope.log'
    if (Test-Path $log) { Write-Host '--- core log ---'; Get-Content $log -Tail 12 }
}
