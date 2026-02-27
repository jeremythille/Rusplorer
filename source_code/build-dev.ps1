#!/usr/bin/env pwsh

# Change to the script's directory
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $scriptDir

$debugDir = ".\target\debug"
$exePath  = "$debugDir\rusplorer-dev.exe"

# Kill rusplorer-dev.exe if it's running (leave production instances alone)
$processes = @(Get-Process rusplorer-dev -ErrorAction SilentlyContinue)
if ($processes.Count -gt 0) {
    $processes | Stop-Process -Force
    foreach ($p in $processes) {
        try { $p.WaitForExit(8000) | Out-Null } catch {}
    }
    Start-Sleep -Milliseconds 500
    Write-Host "Closed running Rusplorer-dev instance(s)"
}

# Delete old dev exe so no stale locked file interferes
Remove-Item $exePath -Force -ErrorAction SilentlyContinue

# Build dev (fast compilation, slower runtime)
Write-Host "Building (dev - fast iteration)..."
cargo build

if ($LASTEXITCODE -eq 0) {
    Write-Host "`nBuild successful!"

    # Copy (not move) to rusplorer-dev.exe so cargo's incremental cache stays intact
    Copy-Item "$debugDir\rusplorer.exe" -Destination $exePath -Force

    # Retry launching — Windows Defender scans new unsigned executables and can
    # cause STATUS_DLL_INIT_FAILED (0xc0000142) during the scan window.
    Write-Host "`nLaunching Rusplorer-dev...`n"
    $launched = $false
    for ($attempt = 1; $attempt -le 5; $attempt++) {
        $proc = Start-Process $exePath -PassThru
        Start-Sleep -Milliseconds 800
        if (!$proc.HasExited) {
            $launched = $true
            break
        }
        Write-Host "  Launch attempt $attempt failed (exit code $($proc.ExitCode)), retrying..."
        Start-Sleep -Milliseconds 1000
    }
    if (-not $launched) { Write-Host "Warning: all launch attempts failed." }
} else {
    Write-Host "Build failed!"
    Read-Host "Press Enter to exit"
}
