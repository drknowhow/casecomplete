param([string]$Exe, [string]$Text)
Add-Type -AssemblyName System.Windows.Forms
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class Win {
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
}
"@
$before = Get-Process notepad -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Id
Start-Process notepad.exe
Start-Sleep -Seconds 2
$np = Get-Process notepad | Where-Object { $before -notcontains $_.Id -and $_.MainWindowHandle -ne 0 } | Select-Object -First 1
if (-not $np) { $np = Get-Process notepad | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1 }
[Win]::SetForegroundWindow($np.MainWindowHandle) | Out-Null
Start-Sleep -Milliseconds 300
& $Exe $Text
Start-Sleep -Milliseconds 800
[System.Windows.Forms.SendKeys]::SendWait("^a")
Start-Sleep -Milliseconds 200
[System.Windows.Forms.SendKeys]::SendWait("^c")
Start-Sleep -Milliseconds 300
Write-Output "GOT: [$(Get-Clipboard -Raw)]"
Stop-Process -Id $np.Id -Force
