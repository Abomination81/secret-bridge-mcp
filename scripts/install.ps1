$ErrorActionPreference = "Stop"

$projectDir = Split-Path -Parent $PSScriptRoot
$installDir = if ($env:SECRET_BRIDGE_INSTALL_DIR) {
    $env:SECRET_BRIDGE_INSTALL_DIR
} else {
    Join-Path $env:LOCALAPPDATA "SecretBridge"
}

cargo build --release --locked --manifest-path (Join-Path $projectDir "Cargo.toml")
New-Item -ItemType Directory -Force -Path $installDir | Out-Null
$destination = Join-Path $installDir "secret-bridge-mcp.exe"
if (Test-Path $destination) {
    $item = Get-Item -Force $destination
    if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "Refusing to replace a reparse point at $destination"
    }
}
$staged = Join-Path $installDir (".secret-bridge-mcp-" + [Guid]::NewGuid().ToString("N") + ".exe")
try {
    Copy-Item (Join-Path $projectDir "target\release\secret-bridge-mcp.exe") $staged
    Move-Item -Force $staged $destination
} finally {
    if (Test-Path $staged) {
        Remove-Item -Force $staged
    }
}

Write-Output "Installed SecretBridge at $destination"
