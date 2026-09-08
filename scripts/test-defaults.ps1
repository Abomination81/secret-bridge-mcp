$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
$testRoot = Join-Path ([IO.Path]::GetTempPath()) ("secretbridge-defaults-tests-" + [Guid]::NewGuid().ToString("N"))
$codexTestDir = Join-Path $testRoot "codex"
$claudeTestDir = Join-Path $testRoot "claude"
$codexFile = Join-Path $codexTestDir "AGENTS.md"
$overrideFile = Join-Path $codexTestDir "AGENTS.override.md"
$claudeFile = Join-Path $claudeTestDir "CLAUDE.md"
$utf8 = New-Object System.Text.UTF8Encoding($false, $true)
$runner = (Get-Process -Id $PID).Path
$setup = Join-Path $PSScriptRoot "enable-defaults.ps1"

function Invoke-TestSetup([string]$Client = "both", [bool]$ExpectFailure = $false) {
    # Windows PowerShell 5.1 promotes redirected native stderr to its error
    # stream. Capture expected refusals without Stop aborting before the exit
    # status can be checked, and restore strict failure handling afterward.
    $previousErrorActionPreference = $ErrorActionPreference
    try {
        $ErrorActionPreference = "Continue"
        $result = & $runner -NoProfile -NonInteractive -File $setup $Client -CodexDir $codexTestDir -ClaudeDir $claudeTestDir 2>&1
        $exitCode = $LASTEXITCODE
    } finally { $ErrorActionPreference = $previousErrorActionPreference }
    if ($ExpectFailure) {
        if ($exitCode -eq 0) { throw "Setup unexpectedly succeeded." }
        if (($result | Out-String) -notmatch "Malformed SecretBridge markers") {
            throw "Setup failed for an unexpected reason: $result"
        }
    } elseif ($exitCode -ne 0) { throw "Setup failed: $result" }
}
function Assert-Equal($Expected, $Actual, [string]$Description) {
    if ($Expected -cne $Actual) { throw "FAIL: $Description" }
}

try {
    [IO.Directory]::CreateDirectory($codexTestDir) | Out-Null
    [IO.Directory]::CreateDirectory($claudeTestDir) | Out-Null
    [IO.File]::WriteAllText($codexFile, "Existing Codex preferences without a final newline", $utf8)
    [IO.File]::WriteAllText($claudeFile, "Existing Claude preferences`r`n", $utf8)
    Invoke-TestSetup
    $codexFirst = [IO.File]::ReadAllText($codexFile)
    $claudeFirst = [IO.File]::ReadAllText($claudeFile)
    if (-not $codexFirst.StartsWith("Existing Codex preferences without a final newline`n`n")) { throw "FAIL: preference preservation" }
    if (-not $claudeFirst.StartsWith("Existing Claude preferences`r`n`r`n")) { throw "FAIL: CRLF preservation" }
    Invoke-TestSetup
    Assert-Equal $codexFirst ([IO.File]::ReadAllText($codexFile)) "Codex idempotence"
    Assert-Equal $claudeFirst ([IO.File]::ReadAllText($claudeFile)) "Claude idempotence"

    [IO.File]::WriteAllText($overrideFile, "Before`r`n<!-- secret-bridge-mcp:begin -->`r`nOld managed block`r`n<!-- secret-bridge-mcp:end -->`r`nAfter without newline", $utf8)
    Invoke-TestSetup "codex"
    Assert-Equal $codexFirst ([IO.File]::ReadAllText($codexFile)) "override leaves AGENTS.md untouched"
    $overrideFirst = [IO.File]::ReadAllText($overrideFile)
    if (-not $overrideFirst.StartsWith("Before`r`n") -or -not $overrideFirst.EndsWith("After without newline")) { throw "FAIL: outside-block preservation" }
    Invoke-TestSetup "codex"
    Assert-Equal $overrideFirst ([IO.File]::ReadAllText($overrideFile)) "override idempotence"

    $broken = "Unchanged`n<!-- secret-bridge-mcp:begin -->`nBroken block`n"
    [IO.File]::WriteAllText($claudeFile, $broken, $utf8)
    Invoke-TestSetup "both" $true
    Assert-Equal $broken ([IO.File]::ReadAllText($claudeFile)) "malformed target unchanged"
    Assert-Equal $overrideFirst ([IO.File]::ReadAllText($overrideFile)) "all targets validated before replacement"

    [IO.File]::WriteAllText($claudeFile, "<!-- secret-bridge-mcp:begin -->`n<!-- secret-bridge-mcp:end -->`n<!-- secret-bridge-mcp:begin -->`n<!-- secret-bridge-mcp:end -->`n", $utf8)
    Invoke-TestSetup "claude" $true
    [IO.File]::WriteAllText($claudeFile, "<!-- secret-bridge-mcp:end -->`n", $utf8)
    Invoke-TestSetup "claude" $true

    Write-Output "PASS: preservation, CRLF, missing final newline, idempotence, override precedence, and malformed marker refusal."
} finally {
    # This is the exact temporary tree created by this test, never a real profile.
    foreach ($file in @($codexFile, $overrideFile, $claudeFile)) {
        if ([IO.File]::Exists($file)) { [IO.File]::Delete($file) }
    }
    foreach ($directory in @($codexTestDir, $claudeTestDir, $testRoot)) {
        if ([IO.Directory]::Exists($directory)) { [IO.Directory]::Delete($directory) }
    }
}

# Expected-failure subprocesses leave LASTEXITCODE=1. Report success only
# after all assertions and cleanup have completed, including in CI wrappers.
exit 0
