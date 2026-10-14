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
Start-Sleep -Seconds 3
$before = Get-Process notepad -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Id
Start-Process notepad.exe
Start-Sleep -Seconds 2
$np = Get-Process notepad | Where-Object { $before -notcontains $_.Id -and $_.MainWindowHandle -ne 0 } | Select-Object -First 1
if (-not $np) { $np = Get-Process notepad | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1 }
[Win]::SetForegroundWindow($np.MainWindowHandle) | Out-Null
Start-Sleep -Milliseconds 500
$text = "Thank you so much for your help with the"
foreach ($ch in $text.ToCharArray()) {
  [System.Windows.Forms.SendKeys]::SendWait([string]$ch)
  Start-Sleep -Milliseconds 60
}
Start-Sleep -Milliseconds 900
[Win]::Key(9)
Start-Sleep -Milliseconds 1800
[Win]::Key(9)
Start-Sleep -Milliseconds 1800
[Win]::Key(9)
Start-Sleep -Milliseconds 800
[Win]::Key(27)
Start-Sleep -Milliseconds 300
[System.Windows.Forms.SendKeys]::SendWait("^a")
Start-Sleep -Milliseconds 200
[System.Windows.Forms.SendKeys]::SendWait("^c")
Start-Sleep -Milliseconds 300
Write-Output "NOTEPAD_TEXT_BEGIN"
Write-Output (Get-Clipboard -Raw)
Write-Output "NOTEPAD_TEXT_END"
Stop-Process -Id $np.Id -Force
