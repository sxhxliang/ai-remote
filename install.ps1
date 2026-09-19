param(
    [string]$Version = $(if ($env:AI_REMOTE_VERSION) { $env:AI_REMOTE_VERSION } else { 'latest' }),
    [string]$Repository = $(if ($env:AI_REMOTE_REPO) { $env:AI_REMOTE_REPO } else { 'sxhxliang/ai-remote' }),
    [string]$InstallDir = $(if ($env:AI_REMOTE_INSTALL_DIR) { $env:AI_REMOTE_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\ai-remote' }),
    [switch]$NoPath
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) { throw 'Use install.sh on Linux and macOS.' }
if ($Repository -notmatch '^[A-Za-z0-9][A-Za-z0-9_.-]*/[A-Za-z0-9][A-Za-z0-9_.-]*$') { throw 'Repository must use owner/repo format.' }
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

try { $architecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString() }
catch { $architecture = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE } }
switch -Regex ($architecture) {
    '^(Arm64|aarch64)$' { $target = 'aarch64-pc-windows-msvc'; break }
    '^(X64|AMD64|x86_64)$' { $target = 'x86_64-pc-windows-msvc'; break }
    default { throw "Unsupported Windows architecture: $architecture" }
}
if ($Version -eq 'latest') {
    try { $Version = (Invoke-RestMethod -Uri "https://api.github.com/repos/$Repository/releases/latest" -TimeoutSec 30).tag_name }
    catch { throw 'No published release could be found. Check the repository, network, and v* release tag.' }
}
if ($Version -cnotmatch '^v\d+\.\d+\.\d+(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?$') { throw 'Version must be a release tag such as v0.2.1.' }

$installRoot = [IO.Path]::GetFullPath($InstallDir).TrimEnd('\', '/')
if ($installRoot.TrimEnd('\', '/') -eq [IO.Path]::GetPathRoot($installRoot).TrimEnd('\', '/')) { throw 'Cannot install into the root of a drive.' }
$asset = "ai-remote-$target.zip"
$baseUrl = "https://github.com/$Repository/releases/download/$Version"
$temporaryRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\', '/')
$temporary = Join-Path $temporaryRoot ('ai-remote-install-' + [Guid]::NewGuid().ToString('N'))
$stage = $null
try {
    New-Item -ItemType Directory -Path $temporary | Out-Null
    $archivePath = Join-Path $temporary $asset
    $checksumPath = Join-Path $temporary 'checksum'
    Write-Output "Downloading AI Remote $Version ($target)..."
    Invoke-WebRequest -UseBasicParsing -Uri "$baseUrl/$asset" -OutFile $archivePath -TimeoutSec 180
    Invoke-WebRequest -UseBasicParsing -Uri "$baseUrl/$asset.sha256" -OutFile $checksumPath -TimeoutSec 30
    $checksumText = (Get-Content -LiteralPath $checksumPath -Raw).Trim()
    if ($checksumText -cnotmatch '^([a-f0-9]{64})  (\S+)$') { throw 'Invalid checksum file.' }
    if ($Matches[2] -cne $asset) { throw 'Checksum filename does not match this platform.' }
    $expectedHash = $Matches[1]
    if ((Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant() -cne $expectedHash) {
        throw 'SHA-256 verification failed; nothing was installed.'
    }

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($archivePath)
    try {
        foreach ($entry in $zip.Entries) {
            $name = $entry.FullName.Replace('\', '/')
            if ($name -cnotmatch '^ai-remote(?:/|$)' -or $name -match '(^|/)\.\.(/|$)' -or $name.Contains(':')) {
                throw 'Archive contains an unsafe path.'
            }
            if (($entry.ExternalAttributes -shr 16 -band 0xF000) -eq 0xA000) { throw 'Archive symbolic links are not allowed.' }
        }
    } finally { $zip.Dispose() }

    New-Item -ItemType Directory -Path $installRoot -Force | Out-Null
    $stage = Join-Path $installRoot ('.install-' + [Guid]::NewGuid().ToString('N'))
    Expand-Archive -LiteralPath $archivePath -DestinationPath $stage
    $payload = Join-Path $stage 'ai-remote'
    $manifest = Get-Content -LiteralPath (Join-Path $payload 'manifest.json') -Raw | ConvertFrom-Json
    if ($manifest.version -cne $Version -or $manifest.target -cne $target) { throw 'Archive version or target does not match this installation.' }
    foreach ($file in @('bin\home-agent.exe', 'bin\signaling-server.exe', 'bin\turn-server.exe', 'frontend\index.html', 'deploy\home-agent.env.example', 'deploy\signaling.env.example', 'deploy\turn.env.example')) {
        if (!(Test-Path -LiteralPath (Join-Path $payload $file) -PathType Leaf)) { throw "Archive is missing $file." }
    }
    $versionName = "$Version-$target"
    $versionsDirectory = Join-Path $installRoot 'versions'
    New-Item -ItemType Directory -Path $versionsDirectory -Force | Out-Null
    $versionDirectory = Join-Path $versionsDirectory $versionName
    $marker = Join-Path $versionDirectory '.archive-sha256'
    if (Test-Path -LiteralPath $versionDirectory) {
        if (!(Test-Path -LiteralPath $marker) -or (Get-Content -LiteralPath $marker -Raw).Trim() -cne $expectedHash) {
            throw 'This version exists with different contents. Use a new release tag or install directory.'
        }
    } else {
        [IO.File]::WriteAllText((Join-Path $payload '.archive-sha256'), $expectedHash)
        # Both paths are derived from the validated version under this install root.
        if ([IO.Path]::GetFullPath($payload) -ne (Join-Path $stage 'ai-remote') -or [IO.Path]::GetFullPath($versionDirectory) -ne (Join-Path $versionsDirectory $versionName)) { throw 'Invalid installation path.' }
        Move-Item -LiteralPath $payload -Destination $versionDirectory
    }

    $configuration = Join-Path $installRoot 'config'
    $binDirectory = Join-Path $installRoot 'bin'
    New-Item -ItemType Directory -Path $configuration, $binDirectory -Force | Out-Null
    foreach ($name in @('home-agent', 'signaling', 'turn')) {
        $destination = Join-Path $configuration "$name.env"
        if (!(Test-Path -LiteralPath $destination)) { Copy-Item -LiteralPath (Join-Path $versionDirectory "deploy\$name.env.example") -Destination $destination }
    }
    $commands = @{ 'ai-remote-agent' = 'home-agent'; 'ai-remote-signaling' = 'signaling-server'; 'ai-remote-turn' = 'turn-server' }
    foreach ($name in $commands.Keys) {
        $commandPath = Join-Path $binDirectory "$name.cmd"
        $text = '@echo off' + "`r`n" + ('"%~dp0..\versions\{0}\bin\{1}.exe" %*' -f $versionName, $commands[$name]) + "`r`n"
        [IO.File]::WriteAllText($commandPath, $text, [Text.Encoding]::ASCII)
    }
    [IO.File]::WriteAllText((Join-Path $installRoot 'current.json'), ($manifest | ConvertTo-Json))
    if (!$NoPath) {
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        if (($userPath -split ';') -notcontains $binDirectory) {
            [Environment]::SetEnvironmentVariable('Path', ($binDirectory + ';' + $userPath).TrimEnd(';'), 'User')
        }
        if (($env:Path -split ';') -notcontains $binDirectory) { $env:Path = $binDirectory + ';' + $env:Path }
    }
    Write-Output "Installed AI Remote $Version in $installRoot"
    Write-Output "Configuration (existing files preserved): $configuration"
    Write-Output "Check the install: & `"$binDirectory\ai-remote-agent.cmd`" --version"
    Write-Output 'After filling in home-agent.env, run:'
    Write-Output ('powershell.exe -NoProfile -ExecutionPolicy Bypass -File "{0}\scripts\run-home-agent.ps1" -EnvFile "{1}\home-agent.env"' -f $versionDirectory, $configuration)
    Write-Output ""
    Write-Output "================================================================================"
    Write-Output "  AI Remote $Version Installed Successfully! [Zero-Config Mode]"
    Write-Output "================================================================================"
    Write-Output "  [Home PC] Start Agent (Quick Connect):"
    Write-Output "     ai-remote-agent <SIGNALING_WS_URL> <TOKEN>"
    Write-Output "     Example: ai-remote-agent ws://your-vps-ip:8080/ws your-token"
    Write-Output ""
    Write-Output "  [Cloud VPS / Dev] Start Signaling Server (with Web UI):"
    Write-Output "     ai-remote-signaling"
    Write-Output "     (Open http://127.0.0.1:8080/setup for Control Panel & Room Status)"
    Write-Output ""
    Write-Output "  [Background Service] Register Windows Scheduled Task for Auto-Start:"
    Write-Output ('powershell.exe -NoProfile -ExecutionPolicy Bypass -File "{0}\scripts\install-home-agent-task.ps1" -EnvFile "{1}\home-agent.env"' -f $versionDirectory, $configuration)
    Write-Output "================================================================================"
    Write-Output ""
} finally {
    if ($stage -and (Test-Path -LiteralPath $stage)) {
        $resolvedStage = (Resolve-Path -LiteralPath $stage).Path
        if ((Split-Path -Parent $resolvedStage) -ne $installRoot -or (Split-Path -Leaf $resolvedStage) -notlike '.install-*') { throw 'Unsafe staging cleanup path.' }
        Remove-Item -LiteralPath $resolvedStage -Recurse -Force
    }
    if (Test-Path -LiteralPath $temporary) {
        $resolvedTemporary = (Resolve-Path -LiteralPath $temporary).Path
        if ((Split-Path -Parent $resolvedTemporary) -ne $temporaryRoot -or (Split-Path -Leaf $resolvedTemporary) -notlike 'ai-remote-install-*') { throw 'Unsafe download cleanup path.' }
        Remove-Item -LiteralPath $resolvedTemporary -Recurse -Force
    }
}
