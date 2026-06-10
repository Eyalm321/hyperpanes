# Track F smoke: launch an ISOLATED hyperpanes (temp APPDATA, control-file env cleared),
# seeded via HYPERPANES_OPEN=reminders (pane parked, due in ~10s, bell list open).
# Captures: s1 (pending), s2 (fired/overdue), then clicks the row and captures s3 (restored).
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSCommandPath
$exe = Join-Path $root "rs\crates\app\target\debug\hyperpanes.exe"
$out = Join-Path $root "smoke-out"
New-Item -ItemType Directory -Force $out | Out-Null
$tmpAppData = Join-Path $out "appdata"
New-Item -ItemType Directory -Force $tmpAppData | Out-Null

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Win {
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr l);
    public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr l);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint x, uint y, uint d, UIntPtr e);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
    [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT p);
    [DllImport("user32.dll")] public static extern IntPtr GetAncestor(IntPtr h, uint flags);
    public static uint PidAt(int x, int y) {
        POINT p; p.X = x; p.Y = y;
        IntPtr h = GetAncestor(WindowFromPoint(p), 2); // GA_ROOT
        uint pid; GetWindowThreadProcessId(h, out pid);
        return pid;
    }
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
    public static IntPtr Found = IntPtr.Zero;
    public static IntPtr FindByPid(uint pid) {
        Found = IntPtr.Zero;
        EnumWindows((h, l) => {
            uint p; GetWindowThreadProcessId(h, out p);
            if (p == pid && IsWindowVisible(h)) {
                RECT r; GetWindowRect(h, out r);
                if (r.R - r.L > 300 && r.B - r.T > 200) { Found = h; return false; }
            }
            return true;
        }, IntPtr.Zero);
        return Found;
    }
    public static void Click(int x, int y) {
        SetCursorPos(x, y);
        System.Threading.Thread.Sleep(150);
        mouse_event(0x0001, 1, 0, 0, UIntPtr.Zero); // MOVE (nudge so hover registers)
        System.Threading.Thread.Sleep(150);
        mouse_event(0x0002, 0, 0, 0, UIntPtr.Zero); // LEFTDOWN
        System.Threading.Thread.Sleep(120);         // hold so Flickable treats it as a tap
        mouse_event(0x0004, 0, 0, 0, UIntPtr.Zero); // LEFTUP
    }
}
"@

function Shot([IntPtr]$h, [string]$path) {
    $r = New-Object Win+RECT
    [Win]::GetWindowRect($h, [ref]$r) | Out-Null
    $w = $r.R - $r.L; $ht = $r.B - $r.T
    $bmp = New-Object System.Drawing.Bitmap($w, $ht)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($r.L, $r.T, 0, 0, (New-Object System.Drawing.Size($w, $ht)))
    $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $g.Dispose(); $bmp.Dispose()
}

# Launch isolated: temp APPDATA, HYPERPANES_OPEN=reminders, control-file env REMOVED.
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $exe
$psi.UseShellExecute = $false
$psi.EnvironmentVariables['APPDATA'] = $tmpAppData
$psi.EnvironmentVariables['HYPERPANES_OPEN'] = 'reminders'
$psi.EnvironmentVariables.Remove('HYPERPANES_CONTROL_FILE') | Out-Null
$proc = [System.Diagnostics.Process]::Start($psi)
$t0 = Get-Date
Write-Output "PID=$($proc.Id)"

# Find the frameless window by PID (MainWindowHandle is unreliable).
$h = [IntPtr]::Zero
for ($i = 0; $i -lt 40 -and $h -eq [IntPtr]::Zero; $i++) {
    Start-Sleep -Milliseconds 250
    $h = [Win]::FindByPid([uint32]$proc.Id)
}
if ($h -eq [IntPtr]::Zero) { $proc.Kill(); throw "window not found" }
# Pin TOPMOST so screen captures show OUR window and the click can't land on another app
# (the host hyperpanes window overlapped us on a previous run — never drive its input).
[Win]::SetWindowPos($h, [IntPtr](-1), 0, 0, 0, 0, 0x13) | Out-Null  # NOMOVE|NOSIZE|NOACTIVATE
Start-Sleep -Seconds 3   # let the shells spawn + first frames render

Shot $h (Join-Path $out "s1-pending.png")
Write-Output "s1 captured at +$([int]((Get-Date)-$t0).TotalSeconds)s"

# Wait past the 10s due time → the reminder fires (bell danger badge, row accent).
while (((Get-Date) - $t0).TotalSeconds -lt 14) { Start-Sleep -Milliseconds 300 }
Shot $h (Join-Path $out "s2-fired.png")
Write-Output "s2 captured at +$([int]((Get-Date)-$t0).TotalSeconds)s"

# Click the first reminder row to restore the pane. Geometry (logical px → DPI-scaled):
# panel spans x = [railLeft-268, railLeft-8] with railLeft = right - 44; first row center
# ~ (32 topbar + 78 panel-y + 8 pad + 18 header + 4 spacing + 20 half-row) = 160 from top.
$dpi = [Win]::GetDpiForWindow($h); $s = $dpi / 96.0
$r = New-Object Win+RECT
[Win]::GetWindowRect($h, [ref]$r) | Out-Null
$cx = [int]($r.R - (44 + 138) * $s)
$cy = [int]($r.T + 160 * $s)
[Win]::SetForegroundWindow($h) | Out-Null
Start-Sleep -Milliseconds 300
# Safety: only click if the pixel under the cursor belongs to OUR (topmost) test window —
# never another app's window. (Focus isn't required for a mouse click to be delivered.)
if ([Win]::PidAt($cx, $cy) -ne [uint32]$proc.Id) {
    $proc.Kill()
    throw "click point is not over the test window - refusing to click"
}
[Win]::Click($cx, $cy)
Write-Output "clicked row at ($cx,$cy) scale=$s rect=($($r.L),$($r.T))-($($r.R),$($r.B))"
Start-Sleep -Seconds 2

Shot $h (Join-Path $out "s3-restored.png")
Write-Output "s3 captured"

# Kill strictly by PID (never by process name).
$proc.Kill()
Write-Output "DONE"
