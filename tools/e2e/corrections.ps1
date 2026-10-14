# Drives Notepad with synthetic keys to check auto-corrections and Left-arrow revert.
# Run with the app started as: CC_ACCEPT_INJECTED=1 casecomplete.exe
param([string]$Text = "I hahve a lleft hand and thhe rulee is easty to usee.")
Add-Type -AssemblyName System.Windows.Forms
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class Win {
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [StructLayout(LayoutKind.Sequential)] public struct KEYBDINPUT { public ushort wVk; public ushort wScan; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
  [StructLayout(LayoutKind.Explicit)] public struct INPUTUNION { [FieldOffset(0)] public KEYBDINPUT ki; [FieldOffset(0)] public long pad1; [FieldOffset(8)] public long pad2; [FieldOffset(16)] public long pad3; [FieldOffset(24)] public long pad4; }
  [StructLayout(LayoutKind.Sequential)] public struct INPUT { public uint type; public INPUTUNION u; }
  [DllImport("user32.dll", SetLastError=true)] public static extern uint SendInput(uint n, INPUT[] inputs, int size);
  public static void Key(ushort vk) {
    var a = new INPUT[2];
    a[0].type = 1; a[0].u.ki.wVk = vk;
    a[1].type = 1; a[1].u.ki.wVk = vk; a[1].u.ki.dwFlags = 2;
    SendInput(2, a, Marshal.SizeOf(typeof(INPUT)));
  }
}
"@
function Read-Notepad {
  [Win]::Key(27); Start-Sleep -Milliseconds 300
  [System.Windows.Forms.SendKeys]::SendWait("^a"); Start-Sleep -Milliseconds 200
  [System.Windows.Forms.SendKeys]::SendWait("^c"); Start-Sleep -Milliseconds 300
  Get-Clipboard -Raw
}
Start-Sleep -Seconds 2
$before = Get-Process notepad -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Id
Start-Process notepad.exe
Start-Sleep -Seconds 2
$np = Get-Process notepad | Where-Object { $before -notcontains $_.Id -and $_.MainWindowHandle -ne 0 } | Select-Object -First 1
if (-not $np) { $np = Get-Process notepad | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1 }
[Win]::SetForegroundWindow($np.MainWindowHandle) | Out-Null
Start-Sleep -Milliseconds 500

# Phase 1: auto-fix while typing
foreach ($ch in $Text.ToCharArray()) {
  [System.Windows.Forms.SendKeys]::SendWait([string]$ch)
  Start-Sleep -Milliseconds 70
}
Start-Sleep -Milliseconds 800
Write-Output "FIXED_BEGIN"
Write-Output (Read-Notepad)
Write-Output "FIXED_END"

# Phase 2: revert with Left arrow right after a fix
[System.Windows.Forms.SendKeys]::SendWait("^a"); Start-Sleep -Milliseconds 100
[Win]::Key(46); Start-Sleep -Milliseconds 200
foreach ($ch in "I thhe ".ToCharArray()) {
  [System.Windows.Forms.SendKeys]::SendWait([string]$ch)
  Start-Sleep -Milliseconds 70
}
Start-Sleep -Milliseconds 400
[Win]::Key(37)
Start-Sleep -Milliseconds 600
Write-Output "REVERTED_BEGIN"
Write-Output (Read-Notepad)
Write-Output "REVERTED_END"
Stop-Process -Id $np.Id -Force
