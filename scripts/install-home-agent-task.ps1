param(
    [Parameter(Mandatory = $true)][string]$EnvFile,
    [string]$Executable,
    [string]$TaskName = 'OllamaRemoteHomeAgent',
    [string]$LogFile = (Join-Path $PSScriptRoot '..\.local\home-agent-service.log')
)

$ErrorActionPreference = 'Stop'
if (!$Executable) {
    $Executable = Join-Path $PSScriptRoot '..\bin\home-agent.exe'
    if (!(Test-Path -LiteralPath $Executable)) { $Executable = Join-Path $PSScriptRoot '..\home-agent\target\release\home-agent.exe' }
}
$agentEnvPath = (Resolve-Path -LiteralPath $EnvFile).Path
$agentExecutable = (Resolve-Path -LiteralPath $Executable).Path
$runner = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot 'run-home-agent.ps1')).Path
$agentLogPath = [IO.Path]::GetFullPath($LogFile)
if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
    throw "Scheduled task '$TaskName' already exists. Remove it explicitly before reinstalling."
}
$powershellPath = Join-Path $PSHOME 'pwsh.exe'
if (!(Test-Path -LiteralPath $powershellPath)) { $powershellPath = Join-Path $PSHOME 'powershell.exe' }
$arguments = '-NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -File "{0}" -EnvFile "{1}" -Executable "{2}" -LogFile "{3}"' -f $runner, $agentEnvPath, $agentExecutable, $agentLogPath
$identity = [Security.Principal.WindowsIdentity]::GetCurrent().Name
$action = New-ScheduledTaskAction -Execute $powershellPath -Argument $arguments -WorkingDirectory (Split-Path -Parent $agentExecutable)
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $identity
$principal = New-ScheduledTaskPrincipal -UserId $identity -LogonType Interactive -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet -StartWhenAvailable -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings | Out-Null
Start-ScheduledTask -TaskName $TaskName
Write-Output "Home Agent starts now and at your next sign-in. Logs: $agentLogPath"
