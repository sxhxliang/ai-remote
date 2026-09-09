$ErrorActionPreference = 'Stop'
$nodeCommand = Get-Command node.exe -ErrorAction SilentlyContinue
if (!$nodeCommand) { throw 'Node.js is required to stop the local stack.' }
& $nodeCommand.Source (Join-Path $PSScriptRoot 'stop-local.mjs')
exit $LASTEXITCODE
