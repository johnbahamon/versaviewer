<#
.SYNOPSIS
  End-to-end smoke test on one Windows machine: signaling server + agent +
  headless viewer, all on 127.0.0.1. Used by CI and for remote testing.

.EXAMPLE
  ./scripts/windows-smoke.ps1                      # default (GPU pipeline if available)
  ./scripts/windows-smoke.ps1 -AgentArgs --no-gpu  # force the CPU pipeline
#>
param(
    [string[]]$AgentArgs = @(),
    [int]$Seconds = 10,
    [string]$BinDir = "target\x86_64-pc-windows-msvc\release",
    [int]$Port = 18080
)
$ErrorActionPreference = "Stop"
Set-Location (Split-Path -Parent $PSScriptRoot)

$logs = "smoke-logs"
New-Item -ItemType Directory -Force $logs | Out-Null
$tag = if ($AgentArgs.Count) { ($AgentArgs -join "_") -replace '[^\w]', '' } else { "default" }
$url = "ws://127.0.0.1:$Port"
$id = "123123123"
$password = "ci-smoke-password"

$env:PORT = "$Port"
$env:HOST = "127.0.0.1"
$sig = Start-Process node -ArgumentList "dist/main.js" -WorkingDirectory "services/signaling" `
    -PassThru -NoNewWindow `
    -RedirectStandardOutput "$logs\signaling-$tag.log" -RedirectStandardError "$logs\signaling-$tag.err"
Start-Sleep -Seconds 2

$env:VV_UNATTENDED_PASSWORD = $password
$agentArgsAll = @("--server", $url, "--id", $id) + $AgentArgs
$agent = Start-Process "$BinDir\vv-agent.exe" -ArgumentList $agentArgsAll `
    -PassThru -NoNewWindow `
    -RedirectStandardOutput "$logs\agent-$tag.log" -RedirectStandardError "$logs\agent-$tag.err"
Start-Sleep -Seconds 3

Write-Host "==== headless viewer ($tag)"
& "$BinDir\examples\headless_viewer.exe" $url $id $password $Seconds 2>&1 |
    Where-Object { $_ -notmatch 'OpenH264' } | Tee-Object -FilePath "$logs\viewer-$tag.log"
$code = $LASTEXITCODE

Start-Sleep -Seconds 1
Stop-Process -Id $agent.Id -Force -ErrorAction SilentlyContinue
Stop-Process -Id $sig.Id -Force -ErrorAction SilentlyContinue

Write-Host "==== agent log ($tag)"
Get-Content "$logs\agent-$tag.log" -Tail 40 | Where-Object { $_ -notmatch 'OpenH264' }
Get-Content "$logs\agent-$tag.err" -Tail 20
Write-Host "==== exit code: $code"
exit $code
