<#
.SYNOPSIS
  Create the Grouper hub VM on this Windows PC with Hyper-V.

.DESCRIPTION
  Downloads Debian 13's official cloud image (checked against Debian's SHA512SUMS),
  turns it into a VHDX, builds a cloud-init seed ISO from user-data.yaml, and creates
  a Generation 2 VM on the Default Switch. First boot installs Docker, git and
  cloudflared and adds a `grouper` user that logs in with -SshKey.

  Needs Hyper-V enabled and membership of "Hyper-V Administrators" (no admin shell),
  plus Python 3 with pycdlib (`pip install pycdlib`).

.EXAMPLE
  .\new-hub-vm.ps1
  ssh -i $HOME\.ssh\grouper_hub_ed25519 grouper@<ip printed at the end>
#>
param(
    [string]$Name = "grouper-hub",
    [string]$Dir = "$HOME\VMs\grouper-hub",
    [string]$SshKey = "$HOME\.ssh\grouper_hub_ed25519",
    [UInt64]$DiskBytes = 40GB,
    [UInt64]$MemoryBytes = 4GB,
    [UInt64]$MaxMemoryBytes = 8GB,
    [int]$Cpus = 4,
    [string]$Switch = "Default Switch"
)
$ErrorActionPreference = "Stop"
$here = $PSScriptRoot
$base = "https://cloud.debian.org/images/cloud/trixie/latest"
$image = "debian-13-genericcloud-amd64.tar.xz"

if (Get-VM -Name $Name -ErrorAction SilentlyContinue) { throw "A VM called $Name already exists." }
New-Item -ItemType Directory -Force $Dir | Out-Null

# SSH key for the grouper user
if (-not (Test-Path $SshKey)) {
    ssh-keygen -t ed25519 -N '""' -C "grouper-hub" -f $SshKey | Out-Null
}
$pub = (Get-Content "$SshKey.pub" -Raw).Trim()

# Image, verified
$archive = Join-Path $Dir $image
if (-not (Test-Path $archive)) {
    Write-Host "Downloading $base/$image"
    curl.exe -fsSL --retry 3 -o $archive "$base/$image"
}
$sums = (curl.exe -fsL "$base/SHA512SUMS") -split "`n"
$want = ($sums | Where-Object { $_ -match "\s$([regex]::Escape($image))$" }) -split '\s+' | Select-Object -First 1
$got = (Get-FileHash $archive -Algorithm SHA512).Hash.ToLower()
if (-not $want -or $got -ne $want) { Remove-Item $archive; throw "Checksum mismatch for $image" }
Write-Host "Checksum OK"

# Disk: raw -> fixed VHD -> dynamic VHDX, grown to $DiskBytes
$raw = Join-Path $Dir "disk.vhd"
$vhdx = Join-Path $Dir "$Name.vhdx"
python "$here\make_disk.py" extract $archive $raw
python "$here\make_disk.py" vhd $raw
Convert-VHD -Path $raw -DestinationPath $vhdx -VHDType Dynamic
Remove-Item $raw
Resize-VHD -Path $vhdx -SizeBytes $DiskBytes

# cloud-init seed
$ud = Join-Path $Dir "user-data"
$md = Join-Path $Dir "meta-data"
(Get-Content "$here\user-data.yaml" -Raw).Replace("__SSH_KEY__", $pub) | Set-Content -NoNewline -Encoding ascii $ud
"instance-id: $Name-$(Get-Date -Format yyyyMMddHHmmss)`nlocal-hostname: $Name`n" | Set-Content -NoNewline -Encoding ascii $md
$seed = Join-Path $Dir "seed.iso"
python "$here\make_disk.py" seed $seed $ud $md

# VM
New-VM -Name $Name -Generation 2 -MemoryStartupBytes $MemoryBytes -VHDPath $vhdx -SwitchName $Switch -Path $Dir | Out-Null
Set-VM -Name $Name -ProcessorCount $Cpus -DynamicMemory -MemoryMinimumBytes 1GB -MemoryMaximumBytes $MaxMemoryBytes `
    -AutomaticStartAction Start -AutomaticStopAction ShutDown -CheckpointType Disabled
Set-VMFirmware -VMName $Name -SecureBootTemplate MicrosoftUEFICertificateAuthority
Add-VMDvdDrive -VMName $Name -Path $seed
Set-VMFirmware -VMName $Name -FirstBootDevice (Get-VMHardDiskDrive -VMName $Name)
Enable-VMIntegrationService -VMName $Name -Name "Guest Service Interface"
Start-VM -Name $Name

Write-Host "Waiting for an IP address (first boot installs packages, a few minutes)..."
$ip = $null
for ($i = 0; $i -lt 120 -and -not $ip; $i++) {
    Start-Sleep 5
    $ip = (Get-VMNetworkAdapter -VMName $Name).IPAddresses | Where-Object { $_ -match '^\d+\.\d+\.\d+\.\d+$' } | Select-Object -First 1
}
if ($ip) {
    Write-Host "VM $Name is at $ip"
    Write-Host "ssh -i $SshKey grouper@$ip"
} else {
    Write-Host "No IP reported yet. Check later with: (Get-VMNetworkAdapter -VMName $Name).IPAddresses"
}
