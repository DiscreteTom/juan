param(
    [Parameter(Mandatory=$true)]
    [string]$Channel,

    [string]$ThreadTs,

    [ValidateSet('plan','timeline')]
    [string]$TaskDisplayMode = 'plan',

    [string]$ConfigPath,

    [int]$DelayMs = 900,

    [string]$RecipientTeamId,

    [string]$RecipientUserId
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Resolve-ConfigPath {
    param([string]$Candidate)

    if ($Candidate -and (Test-Path $Candidate)) {
        return (Resolve-Path $Candidate).Path
    }

    $defaults = @(
        '.\\juan.toml',
        '.\\joan.toml'
    )

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

    $resp = Invoke-RestMethod -Method Post -Uri $uri -Headers $headers -ContentType 'application/json; charset=utf-8' -Body ($Body | ConvertTo-Json -Depth 10)

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

function Resolve-RecipientContext {
    param(
        [string]$Token,
        [string]$Channel,
        [string]$TeamId,
        [string]$UserId
    )

    $resolvedTeam = $TeamId
    $resolvedUser = $UserId

    if (-not $resolvedTeam) {
        $auth = Invoke-SlackApi -Token $Token -Method 'auth.test' -Body @{}
        if ($auth.PSObject.Properties.Name -contains 'team_id') {
            $resolvedTeam = [string]$auth.team_id
        }
    }

    if (-not $resolvedUser) {
        $hist = Invoke-SlackApi -Token $Token -Method 'conversations.history' -Body @{
            channel = $Channel
            limit = 40
        }

        if ($hist.PSObject.Properties.Name -contains 'messages') {
            $msg = $hist.messages | Where-Object {
                ($_.PSObject.Properties.Name -contains 'user') -and
                $_.user -and
                -not (($_.PSObject.Properties.Name -contains 'subtype') -and $_.subtype)
            } | Select-Object -First 1

            if ($msg) {
                $resolvedUser = [string]$msg.user
            }
        }
    }

    if (-not $resolvedTeam -or -not $resolvedUser) {
        throw "Could not resolve recipient_team_id/recipient_user_id. Pass -RecipientTeamId and -RecipientUserId."
    }

    return @{
        team_id = $resolvedTeam
        user_id = $resolvedUser
    }
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
Write-Host "Task display mode: $TaskDisplayMode"

# chat.startStream expects a thread context. If none was provided, create one.
if (-not $ThreadTs) {
    $seed = Invoke-SlackApi -Token $token -Method 'chat.postMessage' -Body @{
        channel = $Channel
        text = 'Plan stream test parent message'
    }
    $ThreadTs = [string]$seed.ts
    Write-Host "Created parent message. thread ts: $ThreadTs"
}

$startBody = @{
    channel = $Channel
    thread_ts = $ThreadTs
    task_display_mode = $TaskDisplayMode
    chunks = @(
        @{ type = 'markdown_text'; text = "*Plan stream test*\n1. start stream\n2. append updates\n3. stop stream" }
    )
}

if ($Channel -match '^[CG]') {
    $recipient = Resolve-RecipientContext -Token $token -Channel $Channel -TeamId $RecipientTeamId -UserId $RecipientUserId
    $startBody.recipient_team_id = $recipient.team_id
    $startBody.recipient_user_id = $recipient.user_id
    Write-Host "Recipient team: $($recipient.team_id)"
    Write-Host "Recipient user: $($recipient.user_id)"
}

$start = Invoke-SlackApi -Token $token -Method 'chat.startStream' -Body $startBody
$streamTs = [string]$start.ts

Write-Host "startStream ok. stream ts: $streamTs"

Start-Sleep -Milliseconds $DelayMs
$append1 = Invoke-SlackApi -Token $token -Method 'chat.appendStream' -Body @{
    channel = $Channel
    ts = $streamTs
    chunks = @(
        @{ type = 'markdown_text'; text = "Update: completed step 1." }
    )
}
Write-Host "appendStream #1 ok"

Start-Sleep -Milliseconds $DelayMs
$append2 = Invoke-SlackApi -Token $token -Method 'chat.appendStream' -Body @{
    channel = $Channel
    ts = $streamTs
    chunks = @(
        @{ type = 'markdown_text'; text = "Update: completed step 2." }
    )
}
Write-Host "appendStream #2 ok"

Start-Sleep -Milliseconds $DelayMs
$stop = Invoke-SlackApi -Token $token -Method 'chat.stopStream' -Body @{
    channel = $Channel
    ts = $streamTs
}
Write-Host "stopStream ok"
Write-Host "Done."
