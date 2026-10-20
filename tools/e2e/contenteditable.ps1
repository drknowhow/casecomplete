# Types sentences with typos into a Chrome contenteditable (the kind of field chat apps
# and Claude desktop use) at a fast cadence, with a quick burst after each space, then
# checks every word that landed: it must be what was typed or that word's allowed fix.
# A joined, split, mangled or wrongly fixed word fails the run.
# -Bare drops the field's textbox role, so UIA reports it as a group, not an editable
# field: the app must leave every word as typed there.
# Run with the app started as: CC_ACCEPT_INJECTED=1 casecomplete.exe
param(
  [string[]]$Lines = @(
    "I wwant to see thhe report from yesterday and aree you sure about it",
    "We should mmore carefully check the throughh traffic this morth and thhat is all",
    "Please haave a look at teh new numbers before the meetting starts",
    "Every one of them came and we waited a while for the quick start",
    "I think it is ok to use Outllook for the calory count",
    "I will go wiht them to recieve the package and definately call you",
    # Typos outside the autocorrect list that are not doubled keys: the model is asked first.
    "I sent the documnet yestreday with the pakage for you"
  ),
  # Typo=fix pairs the app may apply; any other word must land exactly as typed.
  [string[]]$Fixes = @("wwant=want", "thhe=the", "aree=are", "mmore=more", "throughh=through",
    "thhat=that", "haave=have", "teh=the", "meetting=meeting", "Outllook=Outlook",
    "wiht=with", "recieve=receive", "definately=definitely", "documnet=document",
    "yestreday=yesterday", "pakage=package"),
  [int]$CadenceMs = 45,
  [int]$AfterSpaceMs = 12,
  [int]$Runs = 3,
  [switch]$Bare
)
Add-Type -AssemblyName System.Windows.Forms, UIAutomationClient, UIAutomationTypes
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Threading;
public static class Typer {
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
  [StructLayout(LayoutKind.Sequential)] public struct KEYBDINPUT { public ushort wVk; public ushort wScan; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
  [StructLayout(LayoutKind.Explicit)] public struct INPUTUNION { [FieldOffset(0)] public KEYBDINPUT ki; [FieldOffset(0)] public long pad1; [FieldOffset(8)] public long pad2; [FieldOffset(16)] public long pad3; [FieldOffset(24)] public long pad4; }
  [StructLayout(LayoutKind.Sequential)] public struct INPUT { public uint type; public INPUTUNION u; }
  [DllImport("user32.dll", SetLastError=true)] static extern uint SendInput(uint n, INPUT[] inputs, int size);
  static void Send(ushort vk, ushort scan, uint flags) {
    var a = new INPUT[2];
    a[0].type = 1; a[0].u.ki.wVk = vk; a[0].u.ki.wScan = scan; a[0].u.ki.dwFlags = flags;
    a[1].type = 1; a[1].u.ki.wVk = vk; a[1].u.ki.wScan = scan; a[1].u.ki.dwFlags = flags | 2;
    SendInput(2, a, Marshal.SizeOf(typeof(INPUT)));
  }
  // Types `text` one char at a time; false if `target` lost the foreground.
  public static bool Type(IntPtr target, string text, int cadenceMs, int afterSpaceMs) {
    foreach (char c in text) {
      if (GetForegroundWindow() != target) return false;
      if (c == '\n') Send(0x0D, 0, 0); else Send(0, c, 4);
      Thread.Sleep(c == ' ' ? afterSpaceMs : cadenceMs);
    }
    return true;
  }
  public static void Close(IntPtr h) { PostMessage(h, 0x0010, IntPtr.Zero, IntPtr.Zero); }
}
"@
$allowed = @{}
if (-not $Bare) {
  foreach ($pair in $Fixes) { $t, $f = $pair.Split("="); $allowed[$t.ToLower()] = $f.ToLower() }
}
function Bare([string]$w) { return ($w -replace "[^\p{L}\p{N}']", "").ToLower() }

$role = if ($Bare) { "" } else { ' role="textbox" aria-multiline="true"' }
$page = "<title>cc-e2e-ce</title><div contenteditable$role style=`"white-space:pre-wrap;font:16px sans-serif;min-height:400px`"></div><script>document.querySelector(`"div`").focus()</script>"
# Percent-encoded: Start-Process joins its arguments with spaces and does not quote them.
$url = "data:text/html," + [Uri]::EscapeDataString($page)
$expect = if ($Bare) { [System.Windows.Automation.ControlType]::Group } else { [System.Windows.Automation.ControlType]::Edit }
$saved = Get-Clipboard -Raw -ErrorAction SilentlyContinue
$failed = 0
for ($run = 1; $run -le $Runs; $run++) {
  Start-Process chrome.exe -ArgumentList @("--new-window", $url)
  Start-Sleep -Seconds 3
  $w = Get-Process chrome | Where-Object { $_.MainWindowTitle -like "cc-e2e-ce*" } | Select-Object -First 1
  if (-not $w) { Write-Output "RESULT run=${run}: [no chrome window]"; exit 1 }
  $hwnd = $w.MainWindowHandle
  [Typer]::SetForegroundWindow($hwnd) | Out-Null
  Start-Sleep -Milliseconds 800
  $focused = [System.Windows.Automation.AutomationElement]::FocusedElement
  if ([Typer]::GetForegroundWindow() -ne $hwnd -or $focused.Current.ControlType -ne $expect) {
    Write-Output "RESULT run=${run}: [aborted: the test field does not have the focus ($($focused.Current.ControlType.ProgrammaticName))]"
    [Typer]::Close($hwnd); exit 2
  }
  if (-not [Typer]::Type($hwnd, ($Lines -join "`n"), $CadenceMs, $AfterSpaceMs)) {
    Write-Output "RESULT run=${run}: [aborted: the test window lost focus]"
    [Typer]::Close($hwnd); exit 2
  }
  Start-Sleep -Milliseconds 1500
  Set-Clipboard -Value " "
  [System.Windows.Forms.SendKeys]::SendWait("{ESC}"); Start-Sleep -Milliseconds 200
  [System.Windows.Forms.SendKeys]::SendWait("^a"); Start-Sleep -Milliseconds 200
  [System.Windows.Forms.SendKeys]::SendWait("^c"); Start-Sleep -Milliseconds 300
  $got = @((Get-Clipboard -Raw) -split "\r?\n" | Where-Object { $_.Trim() -ne "" })
  [Typer]::Close($hwnd); Start-Sleep -Milliseconds 500
  for ($i = 0; $i -lt $Lines.Count; $i++) {
    $typed = $Lines[$i] -split " +"
    $line = if ($i -lt $got.Count) { $got[$i].Trim() } else { "" }
    $landed = if ($line) { $line -split " +" } else { @() }
    $problems = @()
    if ($landed.Count -ne $typed.Count) { $problems += "typed $($typed.Count) words, $($landed.Count) landed" }
    for ($j = 0; $j -lt [Math]::Min($typed.Count, $landed.Count); $j++) {
      $t = Bare $typed[$j]; $l = Bare $landed[$j]
      if ($l -ne $t -and $l -ne $allowed[$t]) { $problems += "'$($typed[$j])' landed as '$($landed[$j])'" }
    }
    $verdict = if ($problems.Count) { $failed++; "FAIL: " + ($problems -join "; ") } else { "ok" }
    Write-Output "RESULT run=$run line=$($i + 1): $verdict | $line"
  }
}
if ($null -ne $saved) { Set-Clipboard -Value $saved }
$field = if ($Bare) { "group (not editable)" } else { "textbox" }
Write-Output "SUMMARY: $failed line(s) failed in $Runs run(s) in a $field field at ${CadenceMs} ms per key, ${AfterSpaceMs} ms after a space"
exit [int]($failed -gt 0)
