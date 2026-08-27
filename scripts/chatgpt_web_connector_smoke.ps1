# FORK: end-to-end smoke test for the `chatgpt_web` connector mode.
#
# Starts the shared daemon, waits for the tunnel + connector to be ready, drives
# one `codex_exec` turn and one `codex_apply_patch` turn through a real ChatGPT
# web tab, checks the expected markers, then stops the daemon.
#
# Prerequisites: Chrome logged into chatgpt.com with the chrome-mcp daemon
# running (http://127.0.0.1:8848), Developer Mode on. With -Tunnel cloudflared
# (default) cloudflared must be installed; with -Tunnel openai the one-time
# `codex chatgpt-web setup` must already have stored a tunnel id + key.
#
# Usage:
#   pwsh scripts/chatgpt_web_connector_smoke.ps1 [-Codex path\to\codex.exe] `
#       [-Tunnel cloudflared|openai] [-Model chatgpt-web/instant]

[CmdletBinding()]
param(
    [string]$Codex = "codex-rs/target/debug/codex.exe",
    [ValidateSet("cloudflared", "openai")]
    [string]$Tunnel = "cloudflared",
    [string]$Model = "chatgpt-web/instant"
)

$ErrorActionPreference = "Stop"
$common = @("-c", "chatgpt_web.tools=`"connector`"", "-c", "chatgpt_web.tunnel=`"$Tunnel`"")
$failures = 0

function Section($msg) { Write-Host "`n=== $msg ===" -ForegroundColor Cyan }
function Pass($msg) { Write-Host "PASS $msg" -ForegroundColor Green }
function Fail($msg) { Write-Host "FAIL $msg" -ForegroundColor Red; $script:failures++ }

Section "daemon status before"
& $Codex @common chatgpt-web status | Out-Host

Section "reconcile (autostarts the daemon, waits for tunnel + connector)"
$deadline = (Get-Date).AddSeconds(180)
$ready = $false
& $Codex @common chatgpt-web registry reconcile | Out-Host
while ((Get-Date) -lt $deadline) {
    $status = & $Codex @common chatgpt-web status | ConvertFrom-Json
    $health = $status.health
    if ($health -and $health.tunnel_state -eq "ready" -and $health.registry_status -eq "verified") {
        $ready = $true; break
    }
    if ($health -and $health.tunnel_state -like "fatal:*") {
        Fail "tunnel fatal: $($health.tunnel_state)"; break
    }
    Start-Sleep -Seconds 5
}
if ($ready) { Pass "tunnel ready and connector verified" } else { Fail "tunnel/connector not ready within 180s" }

if ($ready) {
    $repo = Join-Path ([System.IO.Path]::GetTempPath()) ("codex-connector-smoke-" + [System.Guid]::NewGuid().ToString("N").Substring(0, 8))
    New-Item -ItemType Directory -Path $repo -Force | Out-Null
    Push-Location $repo
    try {
        Section "turn 1: codex_exec echo CONNECTOR_OK"
        $out = & $Codex @common exec --skip-git-repo-check `
            -c model_provider=chatgpt_web -m $Model `
            -c "chatgpt_web.archive_on_shutdown=false" `
            "Use the codex_exec tool to run the command: echo CONNECTOR_OK - then report the exact output." 2>&1
        if ($out -match "CONNECTOR_OK") { Pass "answer contains CONNECTOR_OK" } else { Fail "CONNECTOR_OK not in answer" }

        Section "turn 2: codex_apply_patch writes hello.txt"
        & $Codex @common exec --skip-git-repo-check `
            -c model_provider=chatgpt_web -m $Model `
            -c "chatgpt_web.archive_on_shutdown=false" `
            "Use the codex_apply_patch tool to create a file named hello.txt whose only contents are the single line HELLO. Then report done." 2>&1 | Out-Host
        $hello = Join-Path $repo "hello.txt"
        if ((Test-Path $hello) -and ((Get-Content $hello -Raw).Trim() -eq "HELLO")) {
            Pass "hello.txt written with HELLO"
        } else {
            Fail "hello.txt missing or wrong contents"
        }
    } finally {
        Pop-Location
        Remove-Item -Recurse -Force $repo -ErrorAction SilentlyContinue
    }

    Section "registry show"
    & $Codex @common chatgpt-web registry show | Out-Host
}

Section "stop daemon"
& $Codex @common chatgpt-web stop | Out-Host

if ($failures -eq 0) {
    Write-Host "`nAll connector smoke checks passed." -ForegroundColor Green
    exit 0
} else {
    Write-Host "`n$failures connector smoke check(s) failed." -ForegroundColor Red
    exit 1
}
