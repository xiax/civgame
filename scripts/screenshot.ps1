<#
.SYNOPSIS
    Screenshot the running CivGame window to C:\CivGame\screenshot.png
#>
param([string]$Path = "C:\CivGame\screenshot.png")

$proc = Get-Process civgame -ErrorAction SilentlyContinue |
        Where-Object { $_.MainWindowHandle -ne 0 } |
        Select-Object -First 1

if (-not $proc) { Write-Host "civgame not running"; exit 1 }

Add-Type @"
using System; using System.Runtime.InteropServices;
public class SS {
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int n);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    public struct RECT { public int L,T,R,B; }
}
"@
$hwnd = $proc.MainWindowHandle
[SS]::ShowWindow($hwnd, 9) | Out-Null
[SS]::SetForegroundWindow($hwnd) | Out-Null
Start-Sleep -Milliseconds 300

$r = New-Object SS+RECT
[SS]::GetWindowRect($hwnd, [ref]$r) | Out-Null
$w = $r.R - $r.L; $h = $r.B - $r.T

Add-Type -AssemblyName System.Windows.Forms, System.Drawing
$bmp = New-Object System.Drawing.Bitmap($w, $h)
$g   = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($r.L, $r.T, 0, 0, (New-Object System.Drawing.Size($w, $h)))
$bmp.Save($Path)
$g.Dispose(); $bmp.Dispose()
Write-Host "screenshot saved: $Path (${w}x${h})"
