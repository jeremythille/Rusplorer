
# Change to the script's directory
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $scriptDir

# Get root path (parent of current directory)
$rootPath = Split-Path -Parent (Get-Location)

# Kill all production instances if any are running
$processes = @(Get-Process -Name rusplorer, rusplorer_compatibility -ErrorAction SilentlyContinue)
if ($processes.Count -gt 0) {
    $count = @($processes).Count
    Stop-Process -InputObject $processes -Force
    # Wait until all instances have fully exited
    $processes | ForEach-Object { $_ | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue }
    Write-Host "Closed $count running Rusplorer instance(s)"
}

# Build both production renderer variants. Cargo writes the same binary name,
# so each one is copied out before the next feature set is compiled.
$builds = @(
    @{ Label = "normal (Windows 11)"; Features = "lean"; Destination = "rusplorer.exe" },
    @{ Label = "compatibility (Windows 10 / software graphics)"; Features = "compatibility"; Destination = "rusplorer_compatibility.exe" }
)
$buildFailed = $false

foreach ($build in $builds) {
    Write-Host "Building prod - $($build.Label)..."
    cargo build --release --no-default-features --features $build.Features
    if ($LASTEXITCODE -ne 0) {
        $buildFailed = $true
        break
    }

    $builtExePath = Join-Path "target/release" "rusplorer.exe"
    if (-not (Test-Path $builtExePath)) {
        Write-Host "Build succeeded but $builtExePath was not produced."
        $buildFailed = $true
        break
    }

    $destinationPath = Join-Path $rootPath $build.Destination
    Copy-Item $builtExePath -Destination $destinationPath -Force
    Write-Host "Binary ready: $destinationPath"
}

if (-not $buildFailed) {
    Write-Host "`nBuild successful!"

    # Keep Cargo's target cache so subsequent builds can reuse compiled artifacts.
    Write-Host "`nLaunching normal Rusplorer...`n"
    Start-Process "$rootPath/rusplorer.exe"
    Write-Host "Rusplorer launched in background. Close this window when done."
    Read-Host "Press Enter to exit"
} else {
    Write-Host "Build failed!"
    Read-Host "Press Enter to exit"
}
