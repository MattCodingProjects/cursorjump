# Builds CursorJump, installs it for the current user, turns on start at
# login and starts it.
#
#   powershell -ExecutionPolicy Bypass -File scripts\win\install.ps1
#
# Installs to %LOCALAPPDATA%\CursorJump\cursorjump.exe. Settings and the
# device identity stay in %APPDATA%\CursorJump and survive reinstalling.

$ErrorActionPreference = 'Stop'

$repo = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$dir = Join-Path $env:LOCALAPPDATA 'CursorJump'
$exe = Join-Path $dir 'cursorjump.exe'

Write-Host 'Building CursorJump (release)...'
Push-Location $repo
try {
    cargo build --release -p cursorjump
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit code $LASTEXITCODE)" }
} finally {
    Pop-Location
}

# A running installed copy locks the file; stop only that one.
$running = Get-Process -Name 'cursorjump' -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -eq $exe }
if ($running) {
    Write-Host 'Stopping the running CursorJump...'
    $running | Stop-Process -Force
    $running | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
}

New-Item -ItemType Directory -Force -Path $dir | Out-Null
Copy-Item -Force (Join-Path $repo 'target\release\cursorjump.exe') $exe
Write-Host "Installed $exe"

# A release build is a GUI program, so wait for it explicitly.
$autostart = Start-Process -FilePath $exe -ArgumentList '--enable-autostart' -Wait -PassThru
if ($autostart.ExitCode -ne 0) { throw "enabling start at login failed (exit code $($autostart.ExitCode))" }

Start-Process -FilePath $exe
Write-Host 'CursorJump is running. Look for its icon in the tray (you may need to click ^ to see it).'
