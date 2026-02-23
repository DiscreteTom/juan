param(
    [Parameter(Mandatory = $true)]
    [string]$Channel,

    [string]$ThreadTs,

    [string]$ConfigPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Resolve-ConfigPath {
    param([string]$Candidate)

    if ($Candidate -and (Test-Path $Candidate)) {
        return (Resolve-Path $Candidate).Path
    }

    $defaults = @('.\juan.toml', '.\joan.toml')
    foreach ($p in $defaults) {
        if (Test-Path $p) {
            return (Resolve-Path $p).Path
        }
    }

    throw "Could not find config file. Pass -ConfigPath, or place juan.toml/joan.toml in current directory."
}

function Get-BotTokenFromToml {
    param([string]$TomlPath)

    $inSlackSection = $false
    foreach ($line in Get-Content -Path $TomlPath) {
        $trimmed = $line.Trim()
        if ($trimmed -match '^\[(.+)\]$') {
            $inSlackSection = ($matches[1] -eq 'slack')
            continue
        }
        if ($inSlackSection -and $trimmed -match '^bot_token\s*=\s*"([^"]+)"') {
            return $matches[1]
        }
    }

    return $null
}

function Invoke-SlackApi {
    param(
        [string]$Token,
        [string]$Method,
        [hashtable]$Body
    )

    $uri = "https://slack.com/api/$Method"
    $headers = @{ Authorization = "Bearer $Token" }

    $resp = Invoke-RestMethod -Method Post -Uri $uri -Headers $headers -ContentType 'application/json; charset=utf-8' -Body ($Body | ConvertTo-Json -Depth 20)
    if (-not $resp.ok) {
        $err = if ($resp.error) { $resp.error } else { 'unknown_error' }
        $details = ''
        $hasMeta = $resp.PSObject.Properties.Name -contains 'response_metadata'
        if ($hasMeta -and $resp.response_metadata -and $resp.response_metadata.messages) {
            $details = " | " + (($resp.response_metadata.messages -join '; '))
        }
        throw "Slack API $Method failed: $err$details"
    }

    return $resp
}

$config = Resolve-ConfigPath -Candidate $ConfigPath
$token = Get-BotTokenFromToml -TomlPath $config
if (-not $token) {
    $token = $env:SLACK_BOT_TOKEN
}
if (-not $token) {
    throw "No bot token found in [slack].bot_token in $config and SLACK_BOT_TOKEN is empty."
}

Write-Host "Using config: $config"
Write-Host "Channel: $Channel"
if ($ThreadTs) {
    Write-Host "Thread TS: $ThreadTs"
}

$planBlock = @{
    type = 'plan'
    title = 'Thinking completed'
    tasks = @(
        @{
            task_id = 'call_001'
            title = 'Fetched user profile information'
            status = 'complete'
        },
        @{
            task_id = 'call_002'
            title = 'Checked user permissions'
            status = 'complete'
        },
        @{
            task_id = 'call_003'
            title = 'Generated comprehensive user report'
            status = 'complete'
        }
    )
}

$body = @{
    channel = $Channel
    text = 'Plan block test message'
    blocks = @($planBlock)
}
if ($ThreadTs) {
    $body.thread_ts = $ThreadTs
}

$resp = Invoke-SlackApi -Token $token -Method 'chat.postMessage' -Body $body
Write-Host "chat.postMessage ok"
Write-Host "ts: $($resp.ts)"
