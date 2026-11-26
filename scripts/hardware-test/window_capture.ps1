# Capture only the client area owned by the launched game process.
param([int] $GameProcess, [string] $OutputFile)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System;
using System.Runtime.InteropServices;
public class GameCapture {
    [StructLayout(LayoutKind.Sequential)] public struct Rect { public int Left, Top, Right, Bottom; }
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr hwnd, out Rect rect);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hwnd, IntPtr dc, uint flags);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
}
'@
[void][GameCapture]::SetProcessDPIAware()
$window = (Get-Process -Id $GameProcess).MainWindowHandle
[uint32]$owner = 0
[void][GameCapture]::GetWindowThreadProcessId($window, [ref]$owner)
$rect = New-Object GameCapture+Rect
if ($owner -ne $GameProcess -or ![GameCapture]::GetClientRect($window, [ref]$rect)) { exit 1 }
$w = $rect.Right
$h = $rect.Bottom
if ($w -lt 64 -or $h -lt 64 -or $w -gt 8192 -or $h -gt 8192 -or $w * $h -gt 34000000) { exit 1 }
$image = New-Object System.Drawing.Bitmap($w, $h)
$graphics = [Drawing.Graphics]::FromImage($image)
$dc = $graphics.GetHdc()
try { $ok = [GameCapture]::PrintWindow($window, $dc, 3) } finally { $graphics.ReleaseHdc($dc) }
if (!$ok) { exit 1 }
$scale = [Math]::Min(1, [Math]::Min(960.0 / $w, 540.0 / $h))
$size = New-Object Drawing.Size([Math]::Max(1, [int]($scale * $w)), [Math]::Max(1, [int]($scale * $h)))
$preview = New-Object Drawing.Bitmap($image, $size)
$preview.Save($OutputFile, [Drawing.Imaging.ImageFormat]::Png)
