param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("wait", "click_text", "ctrl_click_text", "double_click_text", "focus_filter", "key", "screenshot", "dump_names")]
    [string]$Action,
    [string]$Text,
    [string]$Key,
    [string]$Path,
    [int]$TimeoutSeconds = 15
)

Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type -AssemblyName System.Drawing

Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class RusplorerInput {
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extraInfo);
    [DllImport("user32.dll")] public static extern void keybd_event(byte key, byte scan, uint flags, UIntPtr extraInfo);
    public const uint LEFTDOWN = 0x0002;
    public const uint LEFTUP = 0x0004;
    public const uint KEYUP = 0x0002;
    public const byte CTRL = 0x11;
}
"@

function Find-RusplorerWindow {
    $processes = Get-Process -Name "rusplorer", "rusplorer-dev" -ErrorAction SilentlyContinue |
        Where-Object { $_.MainWindowHandle -ne 0 } |
        Sort-Object StartTime -Descending
    foreach ($process in $processes) {
        try {
            $candidate = [System.Windows.Automation.AutomationElement]::FromHandle(
                [System.IntPtr]$process.MainWindowHandle)
            if ($candidate.Current.Name -like "Rusplorer*") { return $candidate }
        } catch {
            # The process may exit between enumeration and handle lookup.
        }
    }

    $root = [System.Windows.Automation.AutomationElement]::RootElement
    $condition = New-Object System.Windows.Automation.PropertyCondition(
        [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
        [System.Windows.Automation.ControlType]::Window)
    $windows = $root.FindAll([System.Windows.Automation.TreeScope]::Children, $condition)
    foreach ($candidate in $windows) {
        $name = $candidate.Current.Name
        if ($name -like "Rusplorer*") { return $candidate }
    }
    return $null
}

function Get-RusplorerWindow {
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $window = Find-RusplorerWindow
        if ($null -ne $window) { return $window }
        Start-Sleep -Milliseconds 200
    } while ((Get-Date) -lt $deadline)
    throw "Rusplorer window not found"
}

function Find-TextElement($window, [string]$value) {
    $condition = New-Object System.Windows.Automation.PropertyCondition(
        [System.Windows.Automation.AutomationElement]::NameProperty, $value)
    $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, $condition)
    if ($elements.Count -eq 0) { throw "Visible UI element not found: $value" }
    return $elements[0]
}

function Find-FilterEdit($window) {
    $condition = New-Object System.Windows.Automation.PropertyCondition(
        [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
        [System.Windows.Automation.ControlType]::Edit)
    $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, $condition)
    if ($elements.Count -eq 0) { throw "Filter input not found" }
    return $elements[0]
}

function Invoke-ElementClick($element, [bool]$doubleClick) {
    $rect = $element.Current.BoundingRectangle
    if ($rect.Width -le 0 -or $rect.Height -le 0) { throw "Element cannot be clicked" }
    [RusplorerInput]::SetCursorPos(
        [int]($rect.Left + $rect.Width / 2),
        [int]($rect.Top + $rect.Height / 2)) | Out-Null
    [RusplorerInput]::mouse_event([RusplorerInput]::LEFTDOWN, 0, 0, 0, [UIntPtr]::Zero)
    [RusplorerInput]::mouse_event([RusplorerInput]::LEFTUP, 0, 0, 0, [UIntPtr]::Zero)
    if ($doubleClick) {
        Start-Sleep -Milliseconds 80
        [RusplorerInput]::mouse_event([RusplorerInput]::LEFTDOWN, 0, 0, 0, [UIntPtr]::Zero)
        [RusplorerInput]::mouse_event([RusplorerInput]::LEFTUP, 0, 0, 0, [UIntPtr]::Zero)
    }
}

switch ($Action) {
    "wait" {
        $null = Get-RusplorerWindow
        Write-Output "ok"
    }
    "click_text" {
        Invoke-ElementClick (Find-TextElement (Get-RusplorerWindow) $Text) $false
    }
    "ctrl_click_text" {
        $window = Get-RusplorerWindow
        $element = Find-TextElement $window $Text
        [RusplorerInput]::keybd_event([RusplorerInput]::CTRL, 0, 0, [UIntPtr]::Zero)
        try {
            Invoke-ElementClick $element $false
        } finally {
            [RusplorerInput]::keybd_event([RusplorerInput]::CTRL, 0, [RusplorerInput]::KEYUP, [UIntPtr]::Zero)
        }
    }
    "double_click_text" {
        Invoke-ElementClick (Find-TextElement (Get-RusplorerWindow) $Text) $true
    }
    "focus_filter" {
        Invoke-ElementClick (Find-FilterEdit (Get-RusplorerWindow)) $false
    }
    "key" {
        Add-Type -AssemblyName System.Windows.Forms
        [System.Windows.Forms.SendKeys]::SendWait($Key)
    }
    "screenshot" {
        $window = Get-RusplorerWindow
        $rect = $window.Current.BoundingRectangle
        $width = [int][Math]::Max(1, $rect.Width)
        $height = [int][Math]::Max(1, $rect.Height)
        $bitmap = New-Object System.Drawing.Bitmap($width, $height)
        $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
        $graphics.CopyFromScreen([int]$rect.Left, [int]$rect.Top, 0, 0, $bitmap.Size)
        $graphics.Dispose()
        $bitmap.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
        $bitmap.Dispose()
        Write-Output $Path
    }
    "dump_names" {
        $window = Get-RusplorerWindow
        $elements = $window.FindAll(
            [System.Windows.Automation.TreeScope]::Descendants,
            [System.Windows.Automation.Condition]::TrueCondition)
        foreach ($element in $elements) {
            $name = $element.Current.Name
            if (-not [string]::IsNullOrWhiteSpace($name)) {
                Write-Output ($element.Current.ControlType.ProgrammaticName + " | " + $name)
            }
        }
    }
}
