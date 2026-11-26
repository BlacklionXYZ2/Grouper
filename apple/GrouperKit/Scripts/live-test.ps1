[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..')).Path
$PackageRoot = Join-Path $RepoRoot 'apple\GrouperKit'
$ScratchRoot = 'C:\Users\oscar\AppData\Local\Temp\claude\5a1f884a431dffa2\89cc49da-e9f7-449a-bab4-4659735dae6c\scratchpad\swift-live'
$HubData = Join-Path $ScratchRoot 'hub-data'
$Workspace = Join-Path $ScratchRoot 'device-workspaces'
$McpPath = Join-Path $ScratchRoot 'mcp.json'
$HubLog = Join-Path $ScratchRoot 'hub.stdout.log'
$HubErrorLog = Join-Path $ScratchRoot 'hub.stderr.log'
$DeviceLog = Join-Path $ScratchRoot 'device.stdout.log'
$DeviceErrorLog = Join-Path $ScratchRoot 'device.stderr.log'
$HubBase = 'http://127.0.0.1:18112'
$HubBind = '127.0.0.1:18112'
$HubProcess = $null
$DeviceProcess = $null
$OriginalSdkRoot = $env:SDKROOT
$OriginalPath = $env:Path
$OriginalLiveHub = $env:GROUPER_LIVE_HUB
$OriginalLiveToken = $env:GROUPER_LIVE_TOKEN
$OriginalLiveDeviceToken = $env:GROUPER_LIVE_DEVICE_TOKEN

$SwiftRoot = 'C:\Users\oscar\AppData\Local\Programs\Swift'
$SwiftVersion = '6.4.3'
$ToolchainBin = Join-Path $SwiftRoot 'Toolchains\6.4.3+Asserts\usr\bin'
$RuntimeBin = Join-Path $SwiftRoot "Runtimes\$SwiftVersion\usr\bin"
$SdkRoot = Join-Path $SwiftRoot "Platforms\$SwiftVersion\Windows.platform\Developer\SDKs\Windows.sdk"
$XCTestBin = Join-Path $SwiftRoot "Platforms\$SwiftVersion\Windows.platform\Developer\Library\XCTest-$SwiftVersion\usr\bin64"
$TestingBin = Join-Path $SwiftRoot "Platforms\$SwiftVersion\Windows.platform\Developer\Library\Testing-$SwiftVersion\usr\bin64"

function Assert-NativeSuccess {
    param([string]$Action)
    if ($LASTEXITCODE -ne 0) {
        throw "$Action failed with exit code $LASTEXITCODE."
    }
}

function Test-PortOpen {
    $client = [System.Net.Sockets.TcpClient]::new()
    try {
        $connection = $client.ConnectAsync('127.0.0.1', 18112)
        return $connection.Wait(250) -and $client.Connected
    }
    catch {
        return $false
    }
    finally {
        $client.Dispose()
    }
}

function Get-TokenFromOutput {
    param([object[]]$Output)
    $token = $Output | ForEach-Object { $_.ToString().Trim() } |
        Where-Object { $_ -match '^[^.\s]+\.[^.\s]+$' } |
        Select-Object -Last 1
    if (-not $token) {
        throw "Could not find the device token in grouper-hub output:`n$($Output -join [Environment]::NewLine)"
    }
    return $token
}

try {
    if (Test-PortOpen) {
        throw "$HubBind is already in use; refusing to test against a process this script did not start."
    }

    if (Test-Path -LiteralPath $ScratchRoot) {
        $resolvedScratch = (Resolve-Path -LiteralPath $ScratchRoot).Path
        if ($resolvedScratch -ne $ScratchRoot) {
            throw "Refusing to clear unexpected scratch path $resolvedScratch."
        }
        Remove-Item -LiteralPath $resolvedScratch -Recurse -Force
    }
    New-Item -ItemType Directory -Path $HubData, $Workspace -Force | Out-Null

    $script = @{
        delay_ms = 800
        rules = @(
            @{
                system = 'orchestrator of Grouper'
                last = 'Swift live E2E'
                reply = @(
                    @{ type = 'text'; text = 'Starting the live Swift approval thread on the connected device.' },
                    @{
                        type = 'tool_use'
                        name = 'start_thread'
                        input = @{
                            title = 'Approval shell'
                            device = 'Swift Live Device'
                            goal = 'Run the scripted shell check and report its output.'
                            expected_output = 'The exact shell output.'
                        }
                    },
                    @{
                        type = 'tool_use'
                        name = 'start_thread'
                        input = @{
                            title = 'Hub stream'
                            device = 'hub'
                            goal = 'Report that the hub thread ran.'
                            hub_reason = 'This half of the live test deliberately runs on the hub.'
                            expected_output = 'A completion sentence.'
                        }
                    }
                )
            },
            @{
                system = 'orchestrator of Grouper'
                last = 'thread_id'
                reply = @(@{ type = 'text'; text = 'The device thread is running.' })
            },
            @{
                system = 'orchestrator of Grouper'
                last = ' done]'
                reply = @(@{ type = 'text'; text = 'The live Swift thread finished successfully.' })
            },
            @{
                system = 'subagent in Grouper'
                last = '# Brief: Approval shell'
                reply = @(
                    @{
                        type = 'tool_use'
                        name = 'run_shell'
                        input = @{ command = 'Write-Output swift-live-approved' }
                    }
                )
            },
            @{
                system = 'subagent in Grouper'
                last = '# Brief: Hub stream'
                reply = @(@{ type = 'text'; text = 'Done. The scripted hub thread completed.' })
            },
            @{
                system = 'subagent in Grouper'
                last = 'swift-live-approved'
                reply = @(@{ type = 'text'; text = 'Done. The shell returned swift-live-approved.' })
            }
        )
        default = @(@{ type = 'text'; text = '(swift live scripted default)' })
    }
    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($McpPath, (@{ includeSubagentChat = $false } | ConvertTo-Json), $utf8NoBom)

    Push-Location $RepoRoot
    try {
        & cargo build --workspace
        Assert-NativeSuccess 'cargo build --workspace'
    }
    finally {
        Pop-Location
    }

    $HubExe = Join-Path $RepoRoot 'target\debug\grouper-hub.exe'
    $DeviceExe = Join-Path $RepoRoot 'target\debug\grouper.exe'
    $adminOutput = @(& $HubExe --data $HubData device add 'Swift Live Admin' --platform windows --admin 2>&1)
    Assert-NativeSuccess 'admin token registration'
    $AdminToken = Get-TokenFromOutput $adminOutput
    $deviceOutput = @(& $HubExe --data $HubData device add 'Swift Live Device' --platform windows 2>&1)
    Assert-NativeSuccess 'device token registration'
    $DeviceToken = Get-TokenFromOutput $deviceOutput

    $providerOptions = @{ inline = $script } | ConvertTo-Json -Depth 12 -Compress
    # Windows PowerShell removes JSON's quotes while constructing a native
    # command line unless they are escaped for CommandLineToArgvW.
    $providerOptionsArgument = '--options=' + $providerOptions.Replace(' ', '\u0020').Replace('"', '\"')
    & $HubExe --data $HubData provider scripted --kind scripted $providerOptionsArgument
    Assert-NativeSuccess 'scripted provider registration'
    & $HubExe --data $HubData model swift-live --provider scripted --model swift-live-1 --roles 'orchestrator,standard,thread'
    Assert-NativeSuccess 'scripted model registration'

    $HubProcess = Start-Process -FilePath $HubExe `
        -ArgumentList @('--data', $HubData, 'serve', '--bind', $HubBind) `
        -RedirectStandardOutput $HubLog -RedirectStandardError $HubErrorLog `
        -WindowStyle Hidden -PassThru

    $healthy = $false
    for ($attempt = 0; $attempt -lt 100; $attempt++) {
        if ($HubProcess.HasExited) {
            throw "grouper-hub exited early with $($HubProcess.ExitCode). See $HubErrorLog"
        }
        try {
            $response = Invoke-WebRequest -Uri "$HubBase/healthz" -UseBasicParsing -TimeoutSec 1
            if ($response.StatusCode -eq 200 -and $response.Content.Trim() -eq 'ok') {
                $healthy = $true
                break
            }
        }
        catch {
            Start-Sleep -Milliseconds 100
        }
    }
    if (-not $healthy) {
        throw "grouper-hub did not become healthy. See $HubErrorLog"
    }

    $oldDeviceHub = $env:GROUPER_HUB
    $oldDeviceToken = $env:GROUPER_TOKEN
    $oldDeviceWorkspace = $env:GROUPER_WORKSPACE
    try {
        $env:GROUPER_HUB = $HubBase
        $env:GROUPER_TOKEN = $DeviceToken
        $env:GROUPER_WORKSPACE = $Workspace
        $DeviceProcess = Start-Process -FilePath $DeviceExe `
            -ArgumentList @('connect', '--mcp-config', $McpPath) `
            -RedirectStandardOutput $DeviceLog -RedirectStandardError $DeviceErrorLog `
            -WindowStyle Hidden -PassThru
    }
    finally {
        $env:GROUPER_HUB = $oldDeviceHub
        $env:GROUPER_TOKEN = $oldDeviceToken
        $env:GROUPER_WORKSPACE = $oldDeviceWorkspace
    }

    $headers = @{ Authorization = "Bearer $AdminToken" }
    $online = $false
    for ($attempt = 0; $attempt -lt 150; $attempt++) {
        if ($DeviceProcess.HasExited) {
            throw "grouper device exited early with $($DeviceProcess.ExitCode). See $DeviceErrorLog"
        }
        try {
            $devices = Invoke-RestMethod -Uri "$HubBase/v1/devices" -Headers $headers -TimeoutSec 1
            if ($devices | Where-Object { $_.name -eq 'Swift Live Device' -and $_.online }) {
                $online = $true
                break
            }
        }
        catch {
            Start-Sleep -Milliseconds 100
        }
    }
    if (-not $online) {
        throw "grouper device did not come online. See $DeviceErrorLog"
    }

    if (-not (Test-Path -LiteralPath $ToolchainBin) -or -not (Test-Path -LiteralPath $RuntimeBin) -or -not (Test-Path -LiteralPath $SdkRoot)) {
        throw 'Swift 6.4.3 toolchain, runtime, or Windows SDK was not found in the expected installation.'
    }
    $env:SDKROOT = $SdkRoot
    $env:Path = "$RuntimeBin;$XCTestBin;$TestingBin;$ToolchainBin;$env:Path"
    $env:GROUPER_LIVE_HUB = $HubBase
    $env:GROUPER_LIVE_TOKEN = $AdminToken
    $env:GROUPER_LIVE_DEVICE_TOKEN = $DeviceToken

    Push-Location $PackageRoot
    try {
        & (Join-Path $ToolchainBin 'swift.exe') test
        Assert-NativeSuccess 'swift test'
    }
    finally {
        Pop-Location
    }
}
finally {
    foreach ($process in @($DeviceProcess, $HubProcess)) {
        if ($null -ne $process -and -not $process.HasExited) {
            Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
            $process.WaitForExit(5000) | Out-Null
        }
    }
    $env:SDKROOT = $OriginalSdkRoot
    $env:Path = $OriginalPath
    $env:GROUPER_LIVE_HUB = $OriginalLiveHub
    $env:GROUPER_LIVE_TOKEN = $OriginalLiveToken
    $env:GROUPER_LIVE_DEVICE_TOKEN = $OriginalLiveDeviceToken
}
