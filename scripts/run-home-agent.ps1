param(
    [Parameter(Mandatory = $true)][string]$EnvFile,
    [string]$Executable,
    [string]$LogFile
)

$ErrorActionPreference = 'Stop'
if (!$Executable) {
    $Executable = Join-Path $PSScriptRoot '..\bin\home-agent.exe'
    if (!(Test-Path -LiteralPath $Executable)) { $Executable = Join-Path $PSScriptRoot '..\home-agent\target\release\home-agent.exe' }
}
$agentEnvPath = (Resolve-Path -LiteralPath $EnvFile).Path
$agentExecutable = (Resolve-Path -LiteralPath $Executable).Path
$allowedKeys = @(
    'SIGNALING_URL', 'ROOM_ID', 'SIGNALING_TOKEN', 'OLLAMA_BASE', 'ALLOWED_PATHS',
    'STUN_URL', 'TURN_URL', 'TURN_USER', 'TURN_PASS', 'ICE_SERVERS_JSON',
    'FORCE_RELAY', 'REQUEST_TIMEOUT_SECS', 'SSL_CERT_FILE', 'SSL_CERT_DIR', 'RUST_LOG'
)
foreach ($line in Get-Content -LiteralPath $agentEnvPath) {
    $entry = $line.Trim()
    if (!$entry -or $entry.StartsWith('#')) { continue }
    $parts = $entry -split '=', 2
    if ($parts.Count -ne 2 -or $parts[0].Trim() -notin $allowedKeys) {
        throw 'The environment file contains an unsupported setting.'
    }
    $value = $parts[1].Trim()
    if ($value.Length -ge 2 -and (($value.StartsWith('"') -and $value.EndsWith('"')) -or ($value.StartsWith("'") -and $value.EndsWith("'")))) {
        $value = $value.Substring(1, $value.Length - 2)
    }
    [Environment]::SetEnvironmentVariable($parts[0].Trim(), $value, 'Process')
}

if ($LogFile) {
    $agentLogPath = [IO.Path]::GetFullPath($LogFile)
    $agentLogDirectory = Split-Path -Parent $agentLogPath
    if (!(Test-Path -LiteralPath $agentLogDirectory)) {
        New-Item -ItemType Directory -Path $agentLogDirectory -Force | Out-Null
    }
    & $agentExecutable *>> $agentLogPath
} else {
    & $agentExecutable
}
exit $LASTEXITCODE
