# End-to-end smoke test for Grouper (Windows PowerShell 5.1 compatible).
# Exercises the real `grouper-hub` and `grouper` binaries together with no API keys.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File scripts/e2e.ps1 [--no-build] [--keep]

param(
    [switch]$NoBuild,
    [switch]$Keep,
    [int]$TimeoutSec = 60,
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$ExtraArgs
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Parse POSIX-style flags (--no-build, --keep, --timeout N) alongside PowerShell switches.
if ($ExtraArgs) {
    for ($i = 0; $i -lt $ExtraArgs.Count; $i++) {
        switch ($ExtraArgs[$i]) {
            '--no-build' { $NoBuild = $true }
            '-no-build'  { $NoBuild = $true }
            '--keep'     { $Keep = $true }
            '-keep'      { $Keep = $true }
            '--timeout'  {
                $i++
                if ($i -ge $ExtraArgs.Count) { throw '--timeout requires a number of seconds' }
                $TimeoutSec = [int]$ExtraArgs[$i]
            }
            '-h'         { Write-Host 'Usage: scripts/e2e.ps1 [--no-build] [--keep] [--timeout <sec>]'; exit 0 }
            '--help'     { Write-Host 'Usage: scripts/e2e.ps1 [--no-build] [--keep] [--timeout <sec>]'; exit 0 }
            default      { throw "Unknown argument: $($ExtraArgs[$i])" }
        }
    }
}

$Utf8NoBom = New-Object System.Text.UTF8Encoding($false)
try { [Console]::OutputEncoding = $Utf8NoBom } catch {}

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$HubExe   = Join-Path $RepoRoot 'target\debug\grouper-hub.exe'
$CliExe   = Join-Path $RepoRoot 'target\debug\grouper.exe'

$script:ManagedProcs = New-Object System.Collections.ArrayList
$script:StepResults  = New-Object System.Collections.ArrayList
$script:SavedEnv     = @{}
$script:TempRoot     = $null

function Format-WinArg {
    param([string]$Arg)
    if ($null -eq $Arg -or $Arg.Length -eq 0) { return '""' }
    if ($Arg -notmatch '[\s"]') { return $Arg }
    $sb = New-Object System.Text.StringBuilder
    [void]$sb.Append('"')
    $backslashes = 0
    for ($i = 0; $i -lt $Arg.Length; $i++) {
        $ch = $Arg[$i]
        if ($ch -eq '\') {
            $backslashes++
        } elseif ($ch -eq '"') {
            [void]$sb.Append('\', $backslashes * 2 + 1)
            [void]$sb.Append('"')
            $backslashes = 0
        } else {
            if ($backslashes -gt 0) {
                [void]$sb.Append('\', $backslashes)
                $backslashes = 0
            }
            [void]$sb.Append($ch)
        }
    }
    if ($backslashes -gt 0) {
        [void]$sb.Append('\', $backslashes * 2)
    }
    [void]$sb.Append('"')
    return $sb.ToString()
}

function Format-WinArgLine {
    param([string[]]$ArgList)
    if (-not $ArgList -or $ArgList.Count -eq 0) { return '' }
    return (($ArgList | ForEach-Object { Format-WinArg $_ }) -join ' ')
}

function Write-Utf8File {
    param([string]$Path, [string]$Text)
    [System.IO.File]::WriteAllText($Path, $Text, $Utf8NoBom)
}

function Read-SharedText {
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path)) { return '' }
    $fs = [System.IO.File]::Open($Path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
    try {
        $sr = New-Object System.IO.StreamReader($fs, $Utf8NoBom)
        try { return $sr.ReadToEnd() } finally { $sr.Dispose() }
    } finally {
        $fs.Dispose()
    }
}

function Get-FreePort {
    $listener = New-Object System.Net.Sockets.TcpListener([System.Net.IPAddress]::Loopback, 0)
    $listener.Start()
    try {
        return $listener.LocalEndpoint.Port
    } finally {
        $listener.Stop()
    }
}

function Start-BgProcess {
    param(
        [string]$FilePath,
        [string[]]$ArgumentList,
        [string]$StdoutPath,
        [string]$StderrPath,
        [string]$WorkingDirectory = $RepoRoot
    )
    $argLine = Format-WinArgLine $ArgumentList
    $proc = Start-Process -FilePath $FilePath -ArgumentList $argLine -WorkingDirectory $WorkingDirectory `
        -RedirectStandardOutput $StdoutPath -RedirectStandardError $StderrPath -NoNewWindow -PassThru
    $null = $proc.Handle
    [void]$script:ManagedProcs.Add($proc)
    return $proc
}

function Stop-ManagedProcess {
    param($Proc)
    if ($null -eq $Proc) { return }
    try {
        if (-not $Proc.HasExited) {
            $Proc.Kill()
            [void]$Proc.WaitForExit(5000)
        }
    } catch {}
}

$script:RunCounter = 0

function Invoke-CommandWithTimeout {
    param(
        [string]$FilePath,
        [string[]]$ArgumentList,
        [int]$Timeout = $TimeoutSec,
        [string]$WorkingDirectory = $RepoRoot
    )
    $script:RunCounter++
    $outPath = Join-Path $script:LogDir ("cmd_{0}_out.log" -f $script:RunCounter)
    $errPath = Join-Path $script:LogDir ("cmd_{0}_err.log" -f $script:RunCounter)
    $argLine = Format-WinArgLine $ArgumentList
    $proc = Start-Process -FilePath $FilePath -ArgumentList $argLine -WorkingDirectory $WorkingDirectory `
        -RedirectStandardOutput $outPath -RedirectStandardError $errPath -NoNewWindow -PassThru
    $null = $proc.Handle
    [void]$script:ManagedProcs.Add($proc)
    $exited = $proc.WaitForExit($Timeout * 1000)
    if (-not $exited) {
        Stop-ManagedProcess $proc
        $stdout = Read-SharedText $outPath
        $stderr = Read-SharedText $errPath
        throw "Command timed out after ${Timeout}s: $FilePath $argLine`nstdout:`n$stdout`nstderr:`n$stderr"
    }
    $proc.WaitForExit()
    $stdout = Read-SharedText $outPath
    $stderr = Read-SharedText $errPath
    return [PSCustomObject]@{
        ExitCode = $proc.ExitCode
        Stdout   = $stdout
        Stderr   = $stderr
        Combined = ($stdout + "`n" + $stderr).Trim()
    }
}

function Invoke-Grouper {
    param(
        [string[]]$CliArgs,
        [string]$Token = $script:AdminToken,
        [int]$Timeout = $TimeoutSec
    )
    $full = @('--hub', $script:HubUrl, '--token', $Token, '--cache', $script:CacheFile) + $CliArgs
    return Invoke-CommandWithTimeout -FilePath $CliExe -ArgumentList $full -Timeout $Timeout
}

function Invoke-RawHttp {
    param(
        [string]$Method,
        [string]$Url,
        [hashtable]$Headers = @{},
        [object]$Body = $null,
        [string]$ContentType = $null,
        [int]$Timeout = $TimeoutSec
    )
    $req = [System.Net.HttpWebRequest]::Create($Url)
    $req.Method = $Method
    $req.AllowAutoRedirect = $false
    $req.Timeout = $Timeout * 1000
    $req.ReadWriteTimeout = $Timeout * 1000
    if ($ContentType) { $req.ContentType = $ContentType }
    foreach ($k in $Headers.Keys) {
        $req.Headers.Add($k, [string]$Headers[$k])
    }
    if ($null -ne $Body) {
        $bytes = $Utf8NoBom.GetBytes([string]$Body)
        $req.ContentLength = $bytes.Length
        $stream = $req.GetRequestStream()
        try { $stream.Write($bytes, 0, $bytes.Length) } finally { $stream.Dispose() }
    }
    $resp = $null
    try {
        $resp = [System.Net.HttpWebResponse]$req.GetResponse()
    } catch [System.Net.WebException] {
        if ($null -ne $_.Exception.Response) {
            $resp = [System.Net.HttpWebResponse]$_.Exception.Response
        } else {
            throw
        }
    }
    try {
        $status = [int]$resp.StatusCode
        $hdrs = @{}
        foreach ($name in $resp.Headers.AllKeys) {
            $hdrs[$name.ToLowerInvariant()] = ($resp.Headers.GetValues($name) -join "`n")
        }
        $respBody = ''
        $rs = $resp.GetResponseStream()
        if ($null -ne $rs) {
            $sr = New-Object System.IO.StreamReader($rs, $Utf8NoBom)
            try { $respBody = $sr.ReadToEnd() } finally { $sr.Dispose() }
        }
        return [PSCustomObject]@{
            Status  = $status
            Headers = $hdrs
            Body    = $respBody
        }
    } finally {
        if ($null -ne $resp) { $resp.Close() }
    }
}

function Wait-Until {
    param(
        [string]$Description,
        [scriptblock]$Condition,
        [int]$Timeout = $TimeoutSec,
        [int]$IntervalMs = 250
    )
    $deadline = (Get-Date).AddSeconds($Timeout)
    while ((Get-Date) -lt $deadline) {
        $val = & $Condition
        if ($val) { return $val }
        Start-Sleep -Milliseconds $IntervalMs
    }
    throw "Timed out after ${Timeout}s waiting for: $Description"
}

function Invoke-Step {
    param(
        [int]$Number,
        [string]$Name,
        [scriptblock]$Action
    )
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    Write-Host ("==> [{0}/10] {1}" -f $Number, $Name) -ForegroundColor Cyan
    try {
        $detail = & $Action
        $sw.Stop()
        $msg = if ($detail) { [string]$detail } else { 'ok' }
        Write-Host ("    PASS ({0:N1}s): {1}" -f $sw.Elapsed.TotalSeconds, $msg) -ForegroundColor Green
        [void]$script:StepResults.Add([PSCustomObject]@{
            Number  = $Number
            Name    = $Name
            Status  = 'PASS'
            Seconds = [Math]::Round($sw.Elapsed.TotalSeconds, 1)
            Detail  = $msg
        })
    } catch {
        $sw.Stop()
        $errMsg = $_.Exception.Message
        Write-Host ("    FAIL ({0:N1}s): {1}" -f $sw.Elapsed.TotalSeconds, $errMsg) -ForegroundColor Red
        [void]$script:StepResults.Add([PSCustomObject]@{
            Number  = $Number
            Name    = $Name
            Status  = 'FAIL'
            Seconds = [Math]::Round($sw.Elapsed.TotalSeconds, 1)
            Detail  = $errMsg
        })
    }
}

# Build inline scripted provider configuration.
# Trigger tokens are uppercase and distinct from every thread title/summary so no finished
# thread summary can ever re-trigger an earlier orchestrator rule.
$ScriptedInline = @{
    delay_ms = 40
    rules    = @(
        @{
            system = 'helpful personal assistant'
            last   = 'E2E_STD_PING'
            reply  = @(
                @{ type = 'text'; text = 'E2E_STD_PONG: standard chat answered.' }
            )
        },
        @{
            system = 'orchestrator of Grouper'
            last   = '"Online Smoke Task" done]'
            reply  = @(
                @{ type = 'text'; text = 'ORCH_ONLINE_FINAL: Online Smoke Task finished cleanly.' }
            )
        },
        @{
            system = 'orchestrator of Grouper'
            last   = '"Scope Check Task" done]'
            reply  = @(
                @{ type = 'text'; text = 'ORCH_SCOPE_FINAL: Scope Check Task finished cleanly.' }
            )
        },
        @{
            system = 'orchestrator of Grouper'
            last   = '"Offline Handover Task" done]'
            reply  = @(
                @{ type = 'text'; text = 'ORCH_OFFLINE_FINAL: Offline Handover Task handed over and finished.' }
            )
        },
        @{
            system = 'orchestrator of Grouper'
            last   = 'reached its hub checkpoint'
            reply  = @(
                @{ type = 'text'; text = 'ORCH_CHECKPOINT_ACK: hub checkpoint ready, waiting for device.' }
            )
        },
        @{
            system = 'orchestrator of Grouper'
            last   = 'continues on Smoke Device'
            reply  = @(
                @{ type = 'text'; text = 'ORCH_HANDOVER_ACK: thread handed over to device.' }
            )
        },
        @{
            system = 'orchestrator of Grouper'
            last   = '"waiting_device"'
            reply  = @(
                @{ type = 'text'; text = 'ORCH_WAITING_ACK: device offline, hub checkpoint started.' }
            )
        },
        @{
            system = 'orchestrator of Grouper'
            last   = '"thread_id"'
            reply  = @(
                @{ type = 'text'; text = 'ORCH_STARTED_ACK: thread started.' }
            )
        },
        @{
            system = 'orchestrator of Grouper'
            last   = 'TRIGGER_ONLINE_TASK'
            reply  = @(
                @{ type = 'text'; text = 'Starting the device task now.' },
                @{
                    type  = 'tool_use'
                    name  = 'start_thread'
                    input = @{
                        title = 'Online Smoke Task'
                        device = 'Smoke Device'
                        goal = 'Write artifact.txt and run a shell verification command.'
                    }
                }
            )
        },
        @{
            system = 'orchestrator of Grouper'
            last   = 'TRIGGER_SCOPE_TASK'
            reply  = @(
                @{ type = 'text'; text = 'Starting scope check thread on hub.' },
                @{
                    type  = 'tool_use'
                    name  = 'start_thread'
                    input = @{
                        title = 'Scope Check Task'
                        device = 'hub'
                        goal = 'Confirm thread scoping on the hub.'
                    }
                }
            )
        },
        @{
            system = 'orchestrator of Grouper'
            last   = 'TRIGGER_OFFLINE_TASK'
            reply  = @(
                @{ type = 'text'; text = 'Starting offline handover task for Smoke Device.' },
                @{
                    type  = 'tool_use'
                    name  = 'start_thread'
                    input = @{
                        title = 'Offline Handover Task'
                        device = 'Smoke Device'
                        goal = 'Prepare checkpoint on hub then continue and finish on Smoke Device.'
                    }
                }
            )
        },
        @{
            system = 'on the device "Smoke Device"'
            last   = 'E2E_SHELL_VERIFIED'
            reply  = @(
                @{ type = 'text'; text = 'THREAD_ONLINE_SUMMARY: wrote artifact.txt and verified shell.' }
            )
        },
        @{
            system = 'on the device "Smoke Device"'
            last   = 'to artifact.txt'
            reply  = @(
                @{
                    type  = 'tool_use'
                    name  = 'run_shell'
                    input = @{ command = 'echo E2E_SHELL_VERIFIED' }
                }
            )
        },
        @{
            system = 'on the device "Smoke Device"'
            last   = '# Brief: Online Smoke Task'
            reply  = @(
                @{
                    type  = 'tool_use'
                    name  = 'write_file'
                    input = @{
                        path    = 'artifact.txt'
                        content = "grouper-e2e-artifact-content`n"
                    }
                }
            )
        },
        @{
            system = 'on the Grouper hub'
            last   = '# Brief: Scope Check Task'
            reply  = @(
                @{ type = 'text'; text = 'THREAD_SCOPE_SUMMARY: scope check done.' }
            )
        },
        @{
            system = 'on the Grouper hub'
            last   = 'to checkpoint.txt'
            reply  = @(
                @{ type = 'text'; text = 'HUB_CHECKPOINT_REPORT: wrote checkpoint.txt on hub, waiting for device.' }
            )
        },
        @{
            system = 'on the Grouper hub'
            last   = '# Brief: Offline Handover Task'
            reply  = @(
                @{
                    type  = 'tool_use'
                    name  = 'write_file'
                    input = @{
                        path    = 'checkpoint.txt'
                        content = "hub-checkpoint-data`n"
                    }
                }
            )
        },
        @{
            system = 'on the device "Smoke Device"'
            last   = 'hub-checkpoint-data'
            reply  = @(
                @{ type = 'text'; text = 'THREAD_OFFLINE_FINISHED: continued on device and read hub-checkpoint-data.' }
            )
        },
        @{
            system = 'on the device "Smoke Device"'
            last   = 'is available again and this thread continues here'
            reply  = @(
                @{
                    type  = 'tool_use'
                    name  = 'read_file'
                    input = @{ path = 'checkpoint.txt' }
                }
            )
        }
    )
    default  = @(
        @{ type = 'text'; text = 'UNEXPECTED_DEFAULT_REPLY' }
    )
}

$EnvKeysToIsolate = @(
    'GROUPER_HUB', 'GROUPER_TOKEN', 'GROUPER_DATA', 'GROUPER_BIND',
    'GROUPER_MASTER_KEY', 'GROUPER_MASTER_KEY_FILE', 'GROUPER_PROVIDER_KEY',
    'GROUPER_WORKSPACE', 'GROUPER_MCP_CONFIG', 'GROUPER_FOLDERS_CONFIG',
    'GROUPER_CACHE', 'GROUPER_USAGE_PROBES'
)

$script:SetupFailed = $false

try {
    # Save and clear ambient GROUPER_* env vars so the test is completely isolated.
    foreach ($k in $EnvKeysToIsolate) {
        $val = [Environment]::GetEnvironmentVariable($k, 'Process')
        $script:SavedEnv[$k] = $val
        [Environment]::SetEnvironmentVariable($k, $null, 'Process')
    }

    if (-not $NoBuild) {
        Write-Host '==> Building workspace (cargo build --workspace)...' -ForegroundColor Cyan
        Push-Location $RepoRoot
        try {
            & cargo build --workspace
            if ($LASTEXITCODE -ne 0) { throw "cargo build --workspace failed with exit code $LASTEXITCODE" }
        } finally {
            Pop-Location
        }
    }

    if (-not (Test-Path -LiteralPath $HubExe)) { throw "Missing hub binary: $HubExe" }
    if (-not (Test-Path -LiteralPath $CliExe)) { throw "Missing cli binary: $CliExe" }

    # Create isolated temp layout.
    $guid = [Guid]::NewGuid().ToString('N').Substring(0, 10)
    $script:TempRoot      = Join-Path ([System.IO.Path]::GetTempPath()) ("grouper-e2e-$guid")
    $script:DataDir       = Join-Path $script:TempRoot 'data'
    $script:WorkspaceDir  = Join-Path $script:TempRoot 'workspace'
    $script:RestoredDir   = Join-Path $script:TempRoot 'restored'
    $script:LogDir        = Join-Path $script:TempRoot 'logs'
    $script:MasterKeyFile = Join-Path $script:TempRoot 'master.key'
    $script:McpConfigFile = Join-Path $script:TempRoot 'mcp.json'
    $script:CacheFile     = Join-Path $script:TempRoot 'cache.db'
    $script:BackupArchive = Join-Path $script:TempRoot 'backup.tar.gz'

    foreach ($d in @($script:TempRoot, $script:DataDir, $script:WorkspaceDir, $script:RestoredDir, $script:LogDir)) {
        [void][System.IO.Directory]::CreateDirectory($d)
    }

    Write-Utf8File -Path $script:McpConfigFile -Text '{"includeSubagentChat": false}'
    [Environment]::SetEnvironmentVariable('GROUPER_MASTER_KEY_FILE', $script:MasterKeyFile, 'Process')

    $script:Port   = Get-FreePort
    $script:Bind   = "127.0.0.1:$($script:Port)"
    $script:HubUrl = "http://$($script:Bind)"

    Write-Host "==> Temp root: $($script:TempRoot)" -ForegroundColor DarkGray
    Write-Host "==> Hub URL:   $($script:HubUrl)" -ForegroundColor DarkGray

    # Register admin and device tokens.
    $r = Invoke-CommandWithTimeout -FilePath $HubExe -ArgumentList @('--data', $script:DataDir, 'device', 'add', 'Smoke Admin', '--platform', 'windows', '--admin')
    if ($r.ExitCode -ne 0) { throw "Failed to add admin device: $($r.Combined)" }
    $script:AdminToken = (($r.Stdout -split "`r?`n") | Where-Object { $_.Trim() -ne '' } | Select-Object -Last 1).Trim()
    if ($script:AdminToken -notmatch '^([^.]+)\..+$') { throw "Unexpected admin token output: $($r.Stdout)" }
    $script:AdminId = $Matches[1]

    $r = Invoke-CommandWithTimeout -FilePath $HubExe -ArgumentList @('--data', $script:DataDir, 'device', 'add', 'Smoke Device', '--platform', 'windows')
    if ($r.ExitCode -ne 0) { throw "Failed to add worker device: $($r.Combined)" }
    $script:DeviceToken = (($r.Stdout -split "`r?`n") | Where-Object { $_.Trim() -ne '' } | Select-Object -Last 1).Trim()
    if ($script:DeviceToken -notmatch '^([^.]+)\..+$') { throw "Unexpected device token output: $($r.Stdout)" }
    $script:DeviceId = $Matches[1]

    # Configure scripted provider with inline script + dummy API key (so restore exercises decryption too).
    $optionsJson = (@{ inline = $ScriptedInline } | ConvertTo-Json -Depth 20 -Compress)
    $r = Invoke-CommandWithTimeout -FilePath $HubExe -ArgumentList @(
        '--data', $script:DataDir,
        'provider', 'scripted',
        '--kind', 'scripted',
        '--key', 'e2e-dummy-provider-key',
        '--options', $optionsJson
    )
    if ($r.ExitCode -ne 0) { throw "Failed to configure scripted provider: $($r.Combined)" }

    # Configure model for all roles.
    $r = Invoke-CommandWithTimeout -FilePath $HubExe -ArgumentList @(
        '--data', $script:DataDir,
        'model', 'e2e-model',
        '--provider', 'scripted',
        '--model', 'e2e-fake-1',
        '--roles', 'orchestrator,standard,thread'
    )
    if ($r.ExitCode -ne 0) { throw "Failed to configure model: $($r.Combined)" }

    # Start grouper-hub serve.
    $script:HubProc = Start-BgProcess -FilePath $HubExe `
        -ArgumentList @('--data', $script:DataDir, 'serve', '--bind', $script:Bind) `
        -StdoutPath (Join-Path $script:LogDir 'hub_out.log') `
        -StderrPath (Join-Path $script:LogDir 'hub_err.log')

    [void](Wait-Until -Description 'hub /healthz to answer ok' -Condition {
        if ($script:HubProc.HasExited) {
            throw "grouper-hub exited prematurely ($($script:HubProc.ExitCode)): $(Read-SharedText (Join-Path $script:LogDir 'hub_err.log'))"
        }
        try {
            $resp = Invoke-RawHttp -Method 'GET' -Url "$($script:HubUrl)/healthz" -Timeout 5
            return ($resp.Status -eq 200 -and $resp.Body.Trim() -eq 'ok')
        } catch {
            return $false
        }
    })

    # Start grouper connect for Smoke Device.
    $script:DeviceProc = Start-BgProcess -FilePath $CliExe `
        -ArgumentList @('--hub', $script:HubUrl, '--token', $script:DeviceToken, '--cache', $script:CacheFile,
                        'connect', '--workspace', $script:WorkspaceDir, '--mcp-config', $script:McpConfigFile) `
        -StdoutPath (Join-Path $script:LogDir 'device_1_out.log') `
        -StderrPath (Join-Path $script:LogDir 'device_1_err.log')

    [void](Wait-Until -Description 'Smoke Device online with run_shell tool' -Condition {
        if ($script:DeviceProc.HasExited) {
            throw "grouper connect exited prematurely ($($script:DeviceProc.ExitCode)): $(Read-SharedText (Join-Path $script:LogDir 'device_1_err.log'))"
        }
        $d = Invoke-Grouper -CliArgs @('devices') -Timeout 10
        return ($d.ExitCode -eq 0 -and $d.Stdout -match ([regex]::Escape($script:DeviceId) + '.*online.*run_shell'))
    })

    # Shared state across steps.
    $script:StdConvId      = $null
    $script:ProjConvId     = $null
    $script:OnlineThreadId = $null

    # Step 1: /healthz and /v1/hub
    Invoke-Step -Number 1 -Name '/healthz and /v1/hub' -Action {
        $h = Invoke-RawHttp -Method 'GET' -Url "$($script:HubUrl)/healthz"
        if ($h.Status -ne 200 -or $h.Body.Trim() -ne 'ok') {
            throw "/healthz returned $($h.Status): $($h.Body)"
        }
        $infoResp = Invoke-RawHttp -Method 'GET' -Url "$($script:HubUrl)/v1/hub"
        if ($infoResp.Status -ne 200) {
            throw "/v1/hub returned $($infoResp.Status): $($infoResp.Body)"
        }
        $info = $infoResp.Body | ConvertFrom-Json
        if (-not $info.hub_id -or -not $info.hub_version -or ($null -eq $info.protocol_version)) {
            throw "Incomplete /v1/hub payload: $($infoResp.Body)"
        }
        return "hub_id=$($info.hub_id) version=$($info.hub_version) proto=$($info.protocol_version)"
    }

    # Step 2: Standard chat answers
    Invoke-Step -Number 2 -Name 'Standard chat answers' -Action {
        $res = Invoke-Grouper -CliArgs @('chat', '--standard', 'E2E_STD_PING')
        if ($res.ExitCode -ne 0) { throw "grouper chat --standard failed ($($res.ExitCode)): $($res.Combined)" }
        if ($res.Stdout -notmatch 'E2E_STD_PONG') { throw "Missing expected reply in stdout:`n$($res.Stdout)" }
        if ($res.Stdout -match '(?m)^conversation\s+(\S+)') {
            $script:StdConvId = $Matches[1].Trim()
        } else {
            throw "Could not parse conversation id from stdout:`n$($res.Stdout)"
        }
        return "conv=$($script:StdConvId)"
    }

    # Step 3: Project chat -> write_file (Auto) + run_shell (Manual) -> approve -> done -> orchestrator reply
    Invoke-Step -Number 3 -Name 'Project chat, device thread, approval, and final reply' -Action {
        $chatOut = Join-Path $script:LogDir 'step3_chat_out.log'
        $chatErr = Join-Path $script:LogDir 'step3_chat_err.log'
        $chatProc = Start-BgProcess -FilePath $CliExe `
            -ArgumentList @('--hub', $script:HubUrl, '--token', $script:AdminToken, '--cache', $script:CacheFile,
                            'chat', 'TRIGGER_ONLINE_TASK') `
            -StdoutPath $chatOut `
            -StderrPath $chatErr

        $approvalId = Wait-Until -Description 'run_shell approval prompt in grouper chat' -Condition {
            $txt = Read-SharedText $chatOut
            if ($txt -match 'run `grouper approve ([A-Za-z0-9_-]+)`') {
                return $Matches[1]
            }
            if ($chatProc.HasExited) {
                $err = Read-SharedText $chatErr
                throw "grouper chat exited before approval ($($chatProc.ExitCode)):`n$txt`n$err"
            }
            return $null
        }

        $midText = Read-SharedText $chatOut
        if ($midText -notmatch 'write_file') {
            throw "Expected write_file (Auto) before run_shell approval:`n$midText"
        }
        if ($midText -notmatch 'approval [A-Za-z0-9_-]+ on .*: run_shell') {
            throw "Expected approval to be for run_shell:`n$midText"
        }

        $app = Invoke-Grouper -CliArgs @('approve', $approvalId)
        if ($app.ExitCode -ne 0) { throw "grouper approve $approvalId failed: $($app.Combined)" }

        if (-not $chatProc.WaitForExit($TimeoutSec * 1000)) {
            Stop-ManagedProcess $chatProc
            $txt = Read-SharedText $chatOut
            throw "grouper chat did not finish within ${TimeoutSec}s after approval:`n$txt"
        }
        $chatProc.WaitForExit()
        $finalOut = Read-SharedText $chatOut
        $finalErr = Read-SharedText $chatErr
        if ($chatProc.ExitCode -ne 0) {
            throw "grouper chat failed ($($chatProc.ExitCode)):`n$finalOut`n$finalErr"
        }

        if ($finalOut -match '(?m)^conversation\s+(\S+)') { $script:ProjConvId = $Matches[1].Trim() }
        if ($finalOut -match 'thread\s+(t[A-Za-z0-9_-]+)\s+"Online Smoke Task"') { $script:OnlineThreadId = $Matches[1].Trim() }
        if (-not $script:ProjConvId -or -not $script:OnlineThreadId) {
            throw "Could not parse conv/thread ids from output:`n$finalOut"
        }
        if ($finalOut -notmatch ([regex]::Escape("thread $($script:OnlineThreadId) done:"))) {
            throw "Thread $($script:OnlineThreadId) did not report done:`n$finalOut"
        }
        if ($finalOut -notmatch 'ORCH_ONLINE_FINAL') {
            throw "Orchestrator final reply missing:`n$finalOut"
        }
        return "conv=$($script:ProjConvId) thread=$($script:OnlineThreadId) approved=$approvalId"
    }

    # Step 4: grouper files <thread> lists and downloads artifact.txt
    Invoke-Step -Number 4 -Name 'grouper files lists and downloads thread artifact' -Action {
        if (-not $script:OnlineThreadId) { throw 'prerequisite online thread id missing' }
        $listRes = Invoke-Grouper -CliArgs @('files', $script:OnlineThreadId)
        if ($listRes.ExitCode -ne 0) { throw "grouper files list failed: $($listRes.Combined)" }
        if ($listRes.Stdout -notmatch 'artifact\.txt') {
            throw "artifact.txt not listed:`n$($listRes.Stdout)"
        }
        $dlPath = Join-Path $script:TempRoot 'downloaded_artifact.txt'
        $dlRes = Invoke-Grouper -CliArgs @('files', $script:OnlineThreadId, 'artifact.txt', '-o', $dlPath)
        if ($dlRes.ExitCode -ne 0) { throw "grouper files download failed: $($dlRes.Combined)" }
        if (-not (Test-Path -LiteralPath $dlPath)) { throw "Downloaded file missing at $dlPath" }
        $actual = [System.IO.File]::ReadAllText($dlPath, $Utf8NoBom)
        $expected = "grouper-e2e-artifact-content`n"
        if ($actual -ne $expected) {
            throw "Downloaded content mismatch: expected '$($expected.Trim())', got '$($actual.Trim())'"
        }
        return "listed and downloaded artifact.txt ($($actual.Length) bytes)"
    }

    # Step 5: Thread scoping (grouper threads -c <conv>)
    Invoke-Step -Number 5 -Name 'Threads are scoped per conversation' -Action {
        if (-not $script:ProjConvId -or -not $script:OnlineThreadId -or -not $script:StdConvId) {
            throw 'prerequisite conversation/thread ids missing'
        }
        $scopeChat = Invoke-Grouper -CliArgs @('chat', 'TRIGGER_SCOPE_TASK')
        if ($scopeChat.ExitCode -ne 0) { throw "Scope check chat failed: $($scopeChat.Combined)" }
        if ($scopeChat.Stdout -notmatch '(?m)^conversation\s+(\S+)') { throw "Missing scope conv id:`n$($scopeChat.Stdout)" }
        $scopeConvId = $Matches[1].Trim()
        if ($scopeChat.Stdout -notmatch 'thread\s+(t[A-Za-z0-9_-]+)\s+"Scope Check Task"') { throw "Missing scope thread id:`n$($scopeChat.Stdout)" }
        $scopeThreadId = $Matches[1].Trim()

        $projThreads = Invoke-Grouper -CliArgs @('threads', '-c', $script:ProjConvId)
        if ($projThreads.ExitCode -ne 0) { throw "grouper threads -c proj failed: $($projThreads.Combined)" }
        $projLines = @(($projThreads.Stdout -split "`r?`n") | Where-Object { $_.Trim() -ne '' })
        if ($projLines.Count -ne 1 -or $projThreads.Stdout -notmatch [regex]::Escape($script:OnlineThreadId) -or $projThreads.Stdout -match [regex]::Escape($scopeThreadId)) {
            throw "Project conv thread scoping failed:`n$($projThreads.Stdout)"
        }

        $scopeThreads = Invoke-Grouper -CliArgs @('threads', '-c', $scopeConvId)
        if ($scopeThreads.ExitCode -ne 0) { throw "grouper threads -c scope failed: $($scopeThreads.Combined)" }
        $scopeLines = @(($scopeThreads.Stdout -split "`r?`n") | Where-Object { $_.Trim() -ne '' })
        if ($scopeLines.Count -ne 1 -or $scopeThreads.Stdout -notmatch [regex]::Escape($scopeThreadId) -or $scopeThreads.Stdout -match [regex]::Escape($script:OnlineThreadId)) {
            throw "Scope conv thread scoping failed:`n$($scopeThreads.Stdout)"
        }

        $stdThreads = Invoke-Grouper -CliArgs @('threads', '-c', $script:StdConvId)
        if ($stdThreads.ExitCode -ne 0) { throw "grouper threads -c std failed: $($stdThreads.Combined)" }
        if ($stdThreads.Stdout.Trim() -ne '') {
            throw "Expected no threads for standard conv $($script:StdConvId), got:`n$($stdThreads.Stdout)"
        }
        return "proj=1 ($($script:OnlineThreadId)), scope=1 ($scopeThreadId), std=0"
    }

    # Step 6: grouper usage --by model shows calls
    Invoke-Step -Number 6 -Name 'grouper usage --by model shows calls' -Action {
        $u = Invoke-Grouper -CliArgs @('usage', '--by', 'model')
        if ($u.ExitCode -ne 0) { throw "grouper usage failed: $($u.Combined)" }
        if ($u.Stdout -notmatch '(?m)^e2e-fake-1\s+(\d+)\s+') {
            throw "Expected model e2e-fake-1 with call count in usage output:`n$($u.Stdout)"
        }
        $calls = [int]$Matches[1]
        if ($calls -le 0) { throw "Expected > 0 calls, got $calls" }
        return "e2e-fake-1 calls=$calls"
    }

    # Step 7: Memory add and search
    Invoke-Step -Number 7 -Name 'Memory add and search' -Action {
        $needle = 'cobalt-orchid-42'
        $add = Invoke-Grouper -CliArgs @('memory', 'add', "E2E smoke fact: $needle", '--tags', 'e2e,smoke')
        if ($add.ExitCode -ne 0) { throw "grouper memory add failed: $($add.Combined)" }
        if ($add.Stdout -notmatch 'saved #(\d+)') { throw "Unexpected memory add output:`n$($add.Stdout)" }
        $memId = $Matches[1]

        $list = Invoke-Grouper -CliArgs @('memory', 'list', '--search', $needle)
        if ($list.ExitCode -ne 0) { throw "grouper memory list --search failed: $($list.Combined)" }
        if ($list.Stdout -notmatch [regex]::Escape($needle)) {
            throw "Memory search did not find '$needle':`n$($list.Stdout)"
        }
        return "saved #$memId and found '$needle'"
    }

    # Step 8: Web app HTML+CSP, session login cookie, cookie GET /v1/conversations, foreign Origin POST -> 403
    Invoke-Step -Number 8 -Name 'Web app HTML/CSP, session cookie, and CSRF guard' -Action {
        $root = Invoke-RawHttp -Method 'GET' -Url "$($script:HubUrl)/"
        if ($root.Status -ne 200) { throw "GET / returned $($root.Status)" }
        if ($root.Body -notmatch '(?i)<!doctype html|<html') { throw 'GET / did not return HTML' }
        $csp = $root.Headers['content-security-policy']
        if (-not $csp -or $csp -notmatch "default-src 'self'") {
            throw "Missing or unexpected Content-Security-Policy header: '$csp'"
        }

        $loginBody = (@{ token = $script:AdminToken } | ConvertTo-Json -Compress)
        $login = Invoke-RawHttp -Method 'POST' -Url "$($script:HubUrl)/v1/session" `
            -Headers @{ 'X-Grouper' = '1'; 'Origin' = $script:HubUrl } `
            -ContentType 'application/json' -Body $loginBody
        if ($login.Status -ne 200) { throw "POST /v1/session returned $($login.Status): $($login.Body)" }
        $setCookie = $login.Headers['set-cookie']
        if (-not $setCookie -or $setCookie -notmatch '(grouper_session=[^;\s]+)') {
            throw "Set-Cookie missing grouper_session: '$setCookie'"
        }
        $cookiePair = $Matches[1]

        $convs = Invoke-RawHttp -Method 'GET' -Url "$($script:HubUrl)/v1/conversations" `
            -Headers @{ 'Cookie' = $cookiePair }
        if ($convs.Status -ne 200) { throw "Cookie GET /v1/conversations returned $($convs.Status): $($convs.Body)" }
        if ($script:ProjConvId -and $convs.Body -notmatch [regex]::Escape($script:ProjConvId)) {
            throw "Cookie GET /v1/conversations did not include $($script:ProjConvId)"
        }

        $csrfBody = (@{ content = 'csrf-probe' } | ConvertTo-Json -Compress)
        $csrf = Invoke-RawHttp -Method 'POST' -Url "$($script:HubUrl)/v1/memories" `
            -Headers @{ 'Cookie' = $cookiePair; 'X-Grouper' = '1'; 'Origin' = 'https://evil.example' } `
            -ContentType 'application/json' -Body $csrfBody
        if ($csrf.Status -ne 403) {
            throw "Expected 403 for foreign Origin POST, got $($csrf.Status): $($csrf.Body)"
        }
        return 'GET / HTML+CSP ok, session cookie works, foreign Origin POST=403'
    }

    # Step 9: Offline flow (stop device -> waiting_device -> restart device -> hand over & finish)
    Invoke-Step -Number 9 -Name 'Offline device checkpoint, handover, and completion' -Action {
        Stop-ManagedProcess $script:DeviceProc
        [void](Wait-Until -Description 'Smoke Device to appear offline' -Condition {
            $d = Invoke-Grouper -CliArgs @('devices') -Timeout 10
            return ($d.ExitCode -eq 0 -and $d.Stdout -match ([regex]::Escape($script:DeviceId) + '.*offline'))
        })

        $chat = Invoke-Grouper -CliArgs @('chat', 'TRIGGER_OFFLINE_TASK')
        if ($chat.ExitCode -ne 0) { throw "Offline project chat failed: $($chat.Combined)" }
        if ($chat.Stdout -notmatch '(?m)^conversation\s+(\S+)') { throw "Missing offline conv id:`n$($chat.Stdout)" }
        $offlineConvId = $Matches[1].Trim()

        $offlineThreadId = Wait-Until -Description 'thread to become waiting_device' -Condition {
            $t = Invoke-Grouper -CliArgs @('threads', '-c', $offlineConvId) -Timeout 10
            if ($t.ExitCode -eq 0 -and $t.Stdout -match '(?m)^(t[A-Za-z0-9_-]+)\s+waiting_device\b') {
                return $Matches[1]
            }
            return $null
        }

        # Wait for hub checkpoint to complete before bringing the device back so handover is deterministic.
        [void](Wait-Until -Description 'hub checkpoint event for waiting thread' -Condition {
            $l = Invoke-Grouper -CliArgs @('log', $offlineConvId) -Timeout 10
            return ($l.ExitCode -eq 0 -and $l.Stdout -match 'hub checkpoint')
        })

        # Restart Smoke Device.
        $script:DeviceProc = Start-BgProcess -FilePath $CliExe `
            -ArgumentList @('--hub', $script:HubUrl, '--token', $script:DeviceToken, '--cache', $script:CacheFile,
                            'connect', '--workspace', $script:WorkspaceDir, '--mcp-config', $script:McpConfigFile) `
            -StdoutPath (Join-Path $script:LogDir 'device_2_out.log') `
            -StderrPath (Join-Path $script:LogDir 'device_2_err.log')

        [void](Wait-Until -Description 'offline thread to hand over and finish done' -Condition {
            $t = Invoke-Grouper -CliArgs @('threads', '-c', $offlineConvId) -Timeout 10
            if ($t.ExitCode -ne 0) { return $false }
            if ($t.Stdout -notmatch ([regex]::Escape($offlineThreadId) + '\s+done\b')) { return $false }
            $l = Invoke-Grouper -CliArgs @('log', $offlineConvId) -Timeout 10
            return ($l.ExitCode -eq 0 -and $l.Stdout -match 'handed over to' -and $l.Stdout -match 'ORCH_OFFLINE_FINAL')
        })

        return "conv=$offlineConvId thread=$offlineThreadId waiting_device->handed_over->done"
    }

    # Step 10: Backup to .tar.gz, restore into another temp dir, verify device list matches
    Invoke-Step -Number 10 -Name 'Hub backup, restore, and device list parity' -Action {
        $origList = Invoke-CommandWithTimeout -FilePath $HubExe -ArgumentList @('--data', $script:DataDir, 'device', 'list')
        if ($origList.ExitCode -ne 0) { throw "Original device list failed: $($origList.Combined)" }

        $bak = Invoke-CommandWithTimeout -FilePath $HubExe -ArgumentList @('--data', $script:DataDir, 'backup', $script:BackupArchive)
        if ($bak.ExitCode -ne 0) { throw "grouper-hub backup failed: $($bak.Combined)" }
        if (-not (Test-Path -LiteralPath $script:BackupArchive) -or (Get-Item -LiteralPath $script:BackupArchive).Length -le 0) {
            throw "Backup archive missing or empty: $($script:BackupArchive)"
        }

        $res = Invoke-CommandWithTimeout -FilePath $HubExe -ArgumentList @('--data', $script:RestoredDir, 'restore', $script:BackupArchive)
        if ($res.ExitCode -ne 0) { throw "grouper-hub restore failed: $($res.Combined)" }

        $restoredList = Invoke-CommandWithTimeout -FilePath $HubExe -ArgumentList @('--data', $script:RestoredDir, 'device', 'list')
        if ($restoredList.ExitCode -ne 0) { throw "Restored device list failed: $($restoredList.Combined)" }

        $normOrig = (($origList.Stdout -split "`r?`n") | Where-Object { $_.Trim() -ne '' } | Sort-Object) -join "`n"
        $normRest = (($restoredList.Stdout -split "`r?`n") | Where-Object { $_.Trim() -ne '' } | Sort-Object) -join "`n"
        if ($normOrig -ne $normRest) {
            throw "Device lists differ!`nOriginal:`n$normOrig`nRestored:`n$normRest"
        }
        if ($normRest -notmatch [regex]::Escape($script:AdminId) -or $normRest -notmatch [regex]::Escape($script:DeviceId)) {
            throw "Restored device list missing expected ids:`n$normRest"
        }
        return "backup+restore verified ($($script:AdminId), $($script:DeviceId))"
    }
} catch {
    $script:SetupFailed = $true
    Write-Host "FATAL SETUP ERROR: $($_.Exception.Message)" -ForegroundColor Red
} finally {
    # Stop every process we launched.
    foreach ($p in $script:ManagedProcs) {
        Stop-ManagedProcess $p
    }
    Start-Sleep -Milliseconds 250

    # Restore saved environment variables.
    foreach ($k in $script:SavedEnv.Keys) {
        [Environment]::SetEnvironmentVariable($k, $script:SavedEnv[$k], 'Process')
    }

    # Remove temp dir unless --keep was passed.
    if ($script:TempRoot -and (Test-Path -LiteralPath $script:TempRoot)) {
        if ($Keep) {
            Write-Host "`nKept temp dir: $($script:TempRoot)" -ForegroundColor Yellow
        } else {
            $removed = $false
            for ($attempt = 0; $attempt -lt 10; $attempt++) {
                try {
                    Remove-Item -LiteralPath $script:TempRoot -Recurse -Force -ErrorAction Stop
                    $removed = $true
                    break
                } catch {
                    Start-Sleep -Milliseconds 200
                }
            }
            if (-not $removed) {
                Write-Warning "Could not fully remove temp dir: $($script:TempRoot)"
            }
        }
    }
}

Write-Host ''
Write-Host '================ E2E Smoke Test Summary ================'
$passed = 0
$failed = 0
foreach ($row in $script:StepResults) {
    if ($row.Status -eq 'PASS') { $passed++ } else { $failed++ }
    $color = if ($row.Status -eq 'PASS') { 'Green' } else { 'Red' }
    Write-Host ("{0,2}. [{1}] ({2,4:N1}s) {3} -- {4}" -f $row.Number, $row.Status, $row.Seconds, $row.Name, $row.Detail) -ForegroundColor $color
}
Write-Host '========================================================'
Write-Host ("Result: {0} passed, {1} failed (setup_error={2})" -f $passed, $failed, $script:SetupFailed)

if ($script:SetupFailed -or $failed -gt 0 -or $passed -lt 10) {
    exit 1
}
exit 0
