# Types a typo sentence into a Chrome textarea or a cmd prompt in Windows Terminal and
# reads back what landed. Never kills the WindowsTerminal host process (it is shared).
# Run with the app started as: CC_ACCEPT_INJECTED=1 casecomplete.exe
param(
  [ValidateSet("chrome", "cmd")] [string]$Target = "chrome",
  [string]$Text = "I hahve a lleft hand and thhe rulee is easty to usee",
  [int]$CadenceMs = 90
)
Add-Type -AssemblyName System.Windows.Forms
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class Win {
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
}
"@
$script:TargetHwnd = [IntPtr]::Zero
function Type-Text([string]$s) {
  foreach ($ch in $s.ToCharArray()) {
    if ([Win]::GetForegroundWindow() -ne $script:TargetHwnd) {
      Write-Output "RESULT: [aborted: target window lost focus]"
      exit 2
    }
    $k = [string]$ch
    if ("+^%~(){}[]".Contains($k)) { $k = "{" + $k + "}" }
    [System.Windows.Forms.SendKeys]::SendWait($k)
    Start-Sleep -Milliseconds $CadenceMs
  }
}
Start-Sleep -Seconds 2
if ($Target -eq "chrome") {
  $url = 'data:text/html,<title>cc-e2e</title><textarea autofocus cols=90 rows=8></textarea>'
  Start-Process chrome.exe -ArgumentList @("--new-window", $url)
  Start-Sleep -Seconds 3
  $w = Get-Process chrome | Where-Object { $_.MainWindowTitle -like "cc-e2e*" } | Select-Object -First 1
  if (-not $w) { Write-Output "RESULT: [no chrome window]"; exit 1 }
  $script:TargetHwnd = $w.MainWindowHandle
  [Win]::SetForegroundWindow($w.MainWindowHandle) | Out-Null
  Start-Sleep -Milliseconds 600
  Type-Text $Text
  Start-Sleep -Milliseconds 900
  [System.Windows.Forms.SendKeys]::SendWait("{ESC}"); Start-Sleep -Milliseconds 200
  [System.Windows.Forms.SendKeys]::SendWait("^a"); Start-Sleep -Milliseconds 200
  [System.Windows.Forms.SendKeys]::SendWait("^c"); Start-Sleep -Milliseconds 300
  Write-Output "RESULT: [$(Get-Clipboard -Raw)]"
  [System.Windows.Forms.SendKeys]::SendWait("^w")
} else {
  $out = Join-Path $env:TEMP "cc-e2e-cmd.txt"
  Remove-Item $out -ErrorAction SilentlyContinue
  Start-Process wt.exe -ArgumentList @("-w", "new", "cmd.exe", "/k", "title cc-e2e")
  Start-Sleep -Seconds 3
  $w = Get-Process WindowsTerminal | Where-Object { $_.MainWindowTitle -like "cc-e2e*" } | Select-Object -First 1
  if (-not $w) { Write-Output "RESULT: [no terminal window]"; exit 1 }
  $script:TargetHwnd = $w.MainWindowHandle
  [Win]::SetForegroundWindow($w.MainWindowHandle) | Out-Null
  Start-Sleep -Milliseconds 600
  Type-Text ("echo " + $Text)
  Start-Sleep -Milliseconds 900
  [System.Windows.Forms.SendKeys]::SendWait("{ESC}"); Start-Sleep -Milliseconds 200
  Type-Text (" > " + $out)
  [System.Windows.Forms.SendKeys]::SendWait("{ENTER}")
  Start-Sleep -Milliseconds 800
  $got = if (Test-Path $out) { (Get-Content $out -Raw).Trim() } else { "<no file>" }
  Write-Output "RESULT: [$got]"
  Type-Text "exit"
  [System.Windows.Forms.SendKeys]::SendWait("{ENTER}")
}
