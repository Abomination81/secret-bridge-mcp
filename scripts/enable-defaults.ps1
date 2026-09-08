param(
    [ValidateSet("codex", "claude", "both")]
    [string]$Client = "both",
    [string]$CodexDir = $(if ($env:CODEX_HOME) { $env:CODEX_HOME } else { Join-Path $HOME ".codex" }),
    [string]$ClaudeDir = $(if ($env:CLAUDE_CONFIG_DIR) { $env:CLAUDE_CONFIG_DIR } else { Join-Path $HOME ".claude" })
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
$beginMarker = "<!-- secret-bridge-mcp:begin -->"
$endMarker = "<!-- secret-bridge-mcp:end -->"
$utf8 = New-Object System.Text.UTF8Encoding($false, $true)
$stagedFiles = New-Object System.Collections.Generic.List[string]

function Assert-RegularFile([string]$Path) {
    $item = Get-Item -LiteralPath $Path -Force -ErrorAction SilentlyContinue
    if ($null -ne $item) {
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "Refusing to replace a reparse point: $Path"
        }
        if ($item.PSIsContainer) { throw "Expected a regular instruction file: $Path" }
    }
}

function Assert-ConfigurationDirectory([string]$Path) {
    if ([string]::IsNullOrWhiteSpace($Path)) { throw "A client configuration directory cannot be empty." }
    $item = Get-Item -LiteralPath $Path -Force -ErrorAction SilentlyContinue
    if ($null -ne $item -and (
        ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or -not $item.PSIsContainer
    )) { throw "Expected a non-reparse-point client configuration directory: $Path" }
}

function Get-ManagedBlock([string]$Text, [string]$Path) {
    $blocks = 0
    $inside = $false
    $start = -1
    $finish = -1
    # Offsets include line endings so all text outside our block stays byte-for-byte equivalent.
    foreach ($line in [regex]::Matches($Text, "[^\r\n]*(?:\r\n|\n|\r|$)")) {
        $value = $line.Value.TrimEnd([char[]]"`r`n")
        if ($value.Contains("secret-bridge-mcp:begin") -or $value.Contains("secret-bridge-mcp:end")) {
            if ($value -eq $beginMarker -and -not $inside -and $blocks -eq 0) {
                $inside = $true
                $start = $line.Index
            } elseif ($value -eq $endMarker -and $inside) {
                $inside = $false
                $blocks++
                $finish = $line.Index + $line.Length
            } else { throw "Malformed SecretBridge markers; leaving the file unchanged: $Path" }
        }
    }
    if ($inside) { throw "Malformed SecretBridge markers; leaving the file unchanged: $Path" }
    return @{ Start = $start; Finish = $finish; BlockCount = $blocks }
}

function Read-InstructionFile([string]$Path) {
    $reader = New-Object IO.StreamReader($Path, $utf8, $true)
    try {
        $text = $reader.ReadToEnd()
        return @{ Text = $text; Encoding = $reader.CurrentEncoding }
    } finally { $reader.Dispose() }
}

try {
    $templatePath = Join-Path (Split-Path -Parent $PSScriptRoot) "docs/client-instructions.md"
    Assert-RegularFile $templatePath
    $template = [IO.File]::ReadAllText($templatePath, $utf8)
    $templateBlock = Get-ManagedBlock $template $templatePath
    if ($templateBlock.BlockCount -ne 1) { throw "Missing managed block in client instruction template: $templatePath" }

    $targets = New-Object System.Collections.Generic.List[string]
    if ($Client -eq "codex" -or $Client -eq "both") {
        Assert-ConfigurationDirectory $CodexDir
        $overridePath = Join-Path $CodexDir "AGENTS.override.md"
        Assert-RegularFile $overridePath
        if ([IO.File]::Exists($overridePath) -and (Get-Item -LiteralPath $overridePath -Force).Length -gt 0) {
            $targets.Add($overridePath)
        } else { $targets.Add((Join-Path $CodexDir "AGENTS.md")) }
    }
    if ($Client -eq "claude" -or $Client -eq "both") {
        Assert-ConfigurationDirectory $ClaudeDir
        $targets.Add((Join-Path $ClaudeDir "CLAUDE.md"))
    }

    # Validate every target before replacing either client's preferences.
    $updates = @()
    foreach ($target in $targets) {
        Assert-RegularFile $target
        $source = if ([IO.File]::Exists($target)) { Read-InstructionFile $target } else { @{ Text = ""; Encoding = $utf8 } }
        $existing = $source.Text
        $block = Get-ManagedBlock $existing $target
        $newline = if ($existing.Contains("`r`n")) { "`r`n" } else { "`n" }
        $replacement = [regex]::Replace($template.TrimEnd([char[]]"`r`n"), "\r\n|\n|\r", $newline) + $newline
        if ($block.BlockCount -eq 1) {
            $updated = $existing.Substring(0, $block.Start) + $replacement + $existing.Substring($block.Finish)
        } else {
            $separator = if ($existing.Length -eq 0) { "" } elseif ($existing.EndsWith("`n") -or $existing.EndsWith("`r")) { $newline } else { $newline + $newline }
            $updated = $existing + $separator + $replacement
        }
        $updates += @{ Target = $target; Text = $updated; Encoding = $source.Encoding }
    }

    foreach ($update in $updates) {
        $directory = Split-Path -Parent $update.Target
        [IO.Directory]::CreateDirectory($directory) | Out-Null
        $stage = Join-Path $directory (".secret-bridge-instructions-" + [Guid]::NewGuid().ToString("N"))
        $stagedFiles.Add($stage)
        [IO.File]::WriteAllText($stage, $update.Text, $update.Encoding)
        $update.Stage = $stage
    }
    foreach ($update in $updates) {
        Assert-RegularFile $update.Target
        if ([IO.File]::Exists($update.Target)) {
            # File.Replace preserves the destination's ACL on Windows.
            # $null binds to an empty string in PowerShell; pass a real .NET null.
            [IO.File]::Replace($update.Stage, $update.Target, [System.Management.Automation.Language.NullString]::Value)
        } else { [IO.File]::Move($update.Stage, $update.Target) }
        Write-Output "Installed SecretBridge guidance in $($update.Target)"
    }

    Write-Output "Prerequisite: register and enable the SecretBridge MCP server in each client."
    Write-Output "Start new Codex / Claude Code sessions to load the guidance. Tool approval settings are unchanged."
} catch {
    Write-Error -ErrorAction Continue $_
    exit 1
} finally {
    foreach ($stage in $stagedFiles) {
        if ([IO.File]::Exists($stage)) { [IO.File]::Delete($stage) }
    }
}
