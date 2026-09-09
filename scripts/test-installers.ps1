param(
    [Parameter(Mandatory = $true)][string]$Installer,
    [Parameter(Mandatory = $true)][string]$FixtureDirectory,
    [Parameter(Mandatory = $true)][string]$ReleaseVersion,
    [Parameter(Mandatory = $true)][string]$ArchiveName,
    [Parameter(Mandatory = $true)][string]$InstallDir,
    [Parameter(Mandatory = $true)][string]$BinaryVersion,
    [string]$Version = 'latest'
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$global:AiRemoteInstallerFixture = @{ Directory = $FixtureDirectory; Version = $ReleaseVersion; Archive = $ArchiveName }

# Only this test process intercepts downloads. The production installer always
# uses GitHub HTTPS URLs and has no alternate download endpoint.
function Invoke-RestMethod {
    param([string]$Uri, [int]$TimeoutSec)
    if ($Uri -cne 'https://api.github.com/repos/fixture/ai-remote/releases/latest') { throw "Unexpected URL: $Uri" }
    return @{ tag_name = $global:AiRemoteInstallerFixture.Version }
}

function Invoke-WebRequest {
    param([switch]$UseBasicParsing, [string]$Uri, [string]$OutFile, [int]$TimeoutSec)
    $fixture = $global:AiRemoteInstallerFixture
    $expectedBase = 'https://github.com/fixture/ai-remote/releases/download/' + $fixture.Version + '/'
    $name = $Uri.Substring($Uri.LastIndexOf('/') + 1)
    if ($name -cnotin @($fixture.Archive, ($fixture.Archive + '.sha256')) -or $Uri -cne ($expectedBase + $name)) { throw "Unexpected URL: $Uri" }
    Copy-Item -LiteralPath (Join-Path $fixture.Directory $name) -Destination $OutFile
}

$previousUserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$previousProcessPath = $env:Path
& $Installer -Version $Version -Repository 'fixture/ai-remote' -InstallDir $InstallDir -NoPath
if ([Environment]::GetEnvironmentVariable('Path', 'User') -cne $previousUserPath -or $env:Path -cne $previousProcessPath) {
    throw '-NoPath changed PATH.'
}
$commands = @{ 'ai-remote-agent' = 'home-agent'; 'ai-remote-signaling' = 'signaling-server'; 'ai-remote-turn' = 'turn-server' }
foreach ($name in $commands.Keys) {
    $output = & (Join-Path $InstallDir "bin\$name.cmd") --version
    if ($LASTEXITCODE -ne 0 -or ($output -join "`n").Trim() -cne ($commands[$name] + ' ' + $BinaryVersion)) {
        throw "Installed command failed: $name"
    }
}
Write-Output 'All installed commands run successfully; PATH was preserved.'
