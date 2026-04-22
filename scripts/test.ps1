<#
.SYNOPSIS
    Build, relaunch, and screenshot CivGame in one step.
.OUTPUTS
    Saves screenshot to C:\CivGame\screenshot.png
    Exits 0 on success, 1 on build failure.
#>
param(
    [int]$WaitSeconds = 4,          # seconds to wait for window after launch
    [string]$ScreenshotPath = "C:\CivGame\screenshot.png"
)

$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"

# ── 0. Kill old instance before build (exe is locked while running) ───────────
$old = Get-Process civgame -ErrorAction SilentlyContinue
if ($old) {
    Write-Host "[run] stopping old civgame (pid $($old.Id))..." -ForegroundColor Yellow
    $old | Stop-Process -Force
    Start-Sleep -Milliseconds 600
}

# ── 1. Build ──────────────────────────────────────────────────────────────────
Write-Host "[build] running cargo build..." -ForegroundColor Cyan
Push-Location "C:\CivGame"
# Run cargo in a cmd subshell to avoid PS5.1 wrapping stderr as ErrorRecord
$buildOutput = cmd /c "cargo build 2>&1"
$buildExit   = $LASTEXITCODE
Pop-Location

if ($buildExit -ne 0) {
    Write-Host "[build] FAILED" -ForegroundColor Red
    $buildOutput | Select-Object -Last 30 | ForEach-Object { Write-Host $_ }
    exit 1
}

Write-Host "[build] OK" -ForegroundColor Green

# ── 2. Launch ─────────────────────────────────────────────────────────────────
Write-Host "[run] launching civgame..." -ForegroundColor Cyan
Start-Process "C:\CivGame\target\debug\civgame.exe"

# ── 4. Wait for window ────────────────────────────────────────────────────────
Write-Host "[run] waiting ${WaitSeconds}s for window..." -ForegroundColor Cyan
$deadline = (Get-Date).AddSeconds($WaitSeconds + 10)
$proc = $null
while ((Get-Date) -lt $deadline) {
    $proc = Get-Process civgame -ErrorAction SilentlyContinue |
            Where-Object { $_.MainWindowHandle -ne 0 } |
            Select-Object -First 1
    if ($proc) { break }
    Start-Sleep -Milliseconds 300
}

if (-not $proc) {
    Write-Host "[run] window never appeared" -ForegroundColor Red
    exit 1
}

Write-Host "[run] window up (pid $($proc.Id)), waiting ${WaitSeconds}s for world gen..." -ForegroundColor Cyan
Start-Sleep -Seconds $WaitSeconds

# ── 5. Bring to front ─────────────────────────────────────────────────────────
Add-Type @"
using System; using System.Runtime.InteropServices;
public class CivWin {
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int n);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    public struct RECT { public int L, T, R, B; }
}
"@
$hwnd = (Get-Process civgame | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1).MainWindowHandle
[CivWin]::ShowWindow($hwnd, 9) | Out-Null
[CivWin]::SetForegroundWindow($hwnd) | Out-Null
Start-Sleep -Milliseconds 300

# ── 6. Screenshot ─────────────────────────────────────────────────────────────
$rect = New-Object CivWin+RECT
[CivWin]::GetWindowRect($hwnd, [ref]$rect) | Out-Null
$w = $rect.R - $rect.L
$h = $rect.B - $rect.T

Add-Type -AssemblyName System.Windows.Forms, System.Drawing
$bmp = New-Object System.Drawing.Bitmap($w, $h)
$g   = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($rect.L, $rect.T, 0, 0, (New-Object System.Drawing.Size($w, $h)))
$bmp.Save($ScreenshotPath)
$g.Dispose(); $bmp.Dispose()

Write-Host "[screenshot] saved to $ScreenshotPath (${w}x${h})" -ForegroundColor Green
