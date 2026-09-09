param(
    [switch]$SkipInstall,
    [switch]$SkipBuild,
    [switch]$NoBrowser,
    [ValidateRange(1, 65535)][int]$MockPort = 11435
)

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$localDirectory = Join-Path $repoRoot '.local'
$runtimeFile = Join-Path $localDirectory 'runtime.json'
$uiUrl = 'http://127.0.0.1:5173'
$nodeCommand = Get-Command node.exe -ErrorAction SilentlyContinue
if (!$nodeCommand) { throw 'Install Node.js 22.12 or later, then open a new terminal.' }
$nodeVersion = [version]((& $nodeCommand.Source --version).TrimStart('v'))
if ($nodeVersion -lt [version]'22.12.0') { throw 'Node.js 22.12 or later is required.' }

function Read-StackStatus {
    try {
        $runtime = Get-Content -LiteralPath $runtimeFile -Raw | ConvertFrom-Json
        if ($runtime.stopped) { return $null }
        $control = [uri]$runtime.control
        if ($control.Scheme -ne 'http' -or $control.Host -ne '127.0.0.1' -or $control.AbsolutePath -ne '/shutdown') { return $null }
        $statusUrl = [UriBuilder]$control
        $statusUrl.Path = '/status'
        $headers = @{ Authorization = 'Bearer ' + $runtime.controlToken }
        $status = Invoke-RestMethod -Uri $statusUrl.Uri -Headers $headers -TimeoutSec 2
        if ($status.status -eq 'running' -and $status.frontend -eq $uiUrl) { return $runtime }
    } catch { }
    return $null
}

New-Item -ItemType Directory -Path $localDirectory -Force | Out-Null
$launchLock = $null
try {
    try { $launchLock = [IO.File]::Open((Join-Path $localDirectory 'launch.lock'), 'OpenOrCreate', 'ReadWrite', 'None') }
    catch {
        if ($_.Exception.InnerException -is [IO.IOException]) { throw 'Another local startup is in progress. Wait for it to finish.' }
        throw
    }

    if (Read-StackStatus) {
        Write-Host 'Restarting the local services started by this project...'
        & $nodeCommand.Source (Join-Path $PSScriptRoot 'stop-local.mjs')
        if ($LASTEXITCODE -ne 0) { throw 'Unable to stop the previous local stack.' }
    }

    Push-Location $repoRoot
    try {
        if (!$SkipInstall) {
            $npmCommand = Get-Command npm.cmd -ErrorAction SilentlyContinue
            if (!$npmCommand) { throw 'npm.cmd was not found. Reinstall Node.js with npm.' }
            Write-Host '[1/3] Installing locked frontend dependencies...'
            & $npmCommand.Source --prefix (Join-Path $repoRoot 'frontend') ci --no-fund
            if ($LASTEXITCODE -ne 0) { throw 'npm ci failed. Check the error above.' }
        }
        if (!$SkipBuild) {
            $cargoCommand = Get-Command cargo.exe -ErrorAction SilentlyContinue
            if (!$cargoCommand) { throw 'Install Rust stable (MSVC) and Visual Studio C++ Build Tools, then open a new terminal.' }
            Write-Host '[2/3] Building the frontend and Rust services...'
            & $nodeCommand.Source (Join-Path $repoRoot 'frontend\node_modules\vite\bin\vite.js') build (Join-Path $repoRoot 'frontend')
            if ($LASTEXITCODE -ne 0) { throw 'Frontend build failed. Check the error above.' }
            foreach ($crate in @('home-agent', 'signaling-server', 'turn-server')) {
                & $cargoCommand.Source build --locked --manifest-path (Join-Path $repoRoot "$crate\Cargo.toml") --all-targets
                if ($LASTEXITCODE -ne 0) { throw "Build failed: $crate. Check the error above." }
            }
        }
    } finally { Pop-Location }

    Write-Host '[3/3] Starting Mock Ollama, signaling, home Agent, TURN and Web UI...'
    $outputLog = Join-Path $localDirectory 'launcher.log'
    $errorLog = Join-Path $localDirectory 'launcher.error.log'
    $launcherScript = Join-Path $PSScriptRoot 'dev.mjs'
    $arguments = @(('"' + $launcherScript + '"'), '--no-build', '--mock-port', [string]$MockPort)
    $launcher = Start-Process -FilePath $nodeCommand.Source -ArgumentList $arguments -WorkingDirectory $repoRoot -WindowStyle Hidden -RedirectStandardOutput $outputLog -RedirectStandardError $errorLog -PassThru
    $deadline = [DateTime]::UtcNow.AddSeconds(90)
    $runtime = $null
    while ([DateTime]::UtcNow -lt $deadline) {
        $launcher.Refresh()
        if ($launcher.HasExited) {
            Get-Content -LiteralPath $outputLog, $errorLog -Tail 35 -ErrorAction SilentlyContinue | Write-Host
            throw 'Local startup failed. See .local/launcher.error.log.'
        }
        $runtime = Read-StackStatus
        if ($runtime) { break }
        Start-Sleep -Milliseconds 300
    }
    if (!$runtime) { throw 'Startup timed out. Check .local/launcher.log and .local/launcher.error.log.' }
    $models = Invoke-RestMethod -Uri ($runtime.mock + '/api/tags') -TimeoutSec 5
    $browserConfig = Invoke-RestMethod -Uri ($uiUrl + '/__local/config') -TimeoutSec 5
    if (!$models.models.Count -or !$browserConfig.mock -or $browserConfig.token -ne $runtime.settings.token) {
        throw 'The Mock service or browser configuration is not ready.'
    }
    Write-Host ''
    Write-Host ('Web UI:      ' + $uiUrl) -ForegroundColor Green
    Write-Host ('Mock Ollama: ' + $runtime.mock)
    Write-Host 'Settings are filled automatically. Connect, select a model and send a message.'
    Write-Host 'Expected reply: a Chinese response identifying itself as Mock Ollama.'
    Write-Host 'Services keep running after this window closes. Logs are in .local/.'
    Write-Host 'To stop: double-click stop-local.cmd, or run scripts/stop-local.ps1.'
    if (!$NoBrowser) { Start-Process -FilePath $uiUrl }
} finally {
    if ($launchLock) { $launchLock.Dispose() }
}
