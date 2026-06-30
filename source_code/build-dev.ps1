#!/usr/bin/env pwsh

param(
    [switch]$Watch,
    [switch]$Once
)

# Change to the script's directory
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $scriptDir

function Get-SourceStamp {
    $files = @()
    if (Test-Path .\src) {
        $files += Get-ChildItem .\src -Recurse -File -ErrorAction SilentlyContinue
    }
    if (Test-Path .\Cargo.toml) {
        $files += Get-Item .\Cargo.toml
    }
    if (Test-Path .\build.rs) {
        $files += Get-Item .\build.rs
    }
    if ($files.Count -eq 0) {
        return 0L
    }
    return ($files | Measure-Object -Property LastWriteTimeUtc -Maximum).Maximum.Ticks
}

# Default behavior: watch mode, unless -Once is explicitly requested.
if (-not $Once) {
    $Watch = $true
}

if ($Watch) {
    $cargoWatch = Get-Command cargo-watch -ErrorAction SilentlyContinue
    if ($cargoWatch) {
        Write-Host "Starting watch mode (auto rebuild + relaunch on file changes)..."
        Write-Host "Use .\build-dev.ps1 -Once for a single build+launch."
        Write-Host "Keep this terminal open while developing."
        cargo watch -w src -w Cargo.toml -w build.rs -s "powershell -NoProfile -ExecutionPolicy Bypass -File ./build-dev.ps1 -Once"
        exit $LASTEXITCODE
    }

    Write-Host "cargo-watch not found, using built-in watcher fallback."
    Write-Host "Keep this terminal open while developing."

    powershell -NoProfile -ExecutionPolicy Bypass -File .\build-dev.ps1 -Once
    $lastStamp = Get-SourceStamp
    while ($true) {
        Start-Sleep -Milliseconds 700
        $newStamp = Get-SourceStamp
        if ($newStamp -gt $lastStamp) {
            $lastStamp = $newStamp
            Write-Host "Change detected - rebuilding..."
            powershell -NoProfile -ExecutionPolicy Bypass -File .\build-dev.ps1 -Once
        }
    }
}

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
