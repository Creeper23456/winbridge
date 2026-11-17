# SPDX-License-Identifier: GPL-3.0-or-later
<#
.SYNOPSIS
Installs the WinBridge global graphics hook: every program that is not part of Windows gets the
accelerated OpenGL and Direct3D DLLs, except the ones on a deny list.

.DESCRIPTION
The hook (wbhook64.dll, wbhook32.dll; venus/guest/wbhook.c) is an AppInit_DLLs module. It loads Mesa's
OpenGL (opengl32.dll, libgallium_wgl.dll) and DXVK (d3d9, d3d10core, d3d11, dxgi) from the folder
next to it and makes the program use them, without touching the program's own folder. Read the
header of wbhook.c for what it can and cannot reach: programs that run in a sandbox which forbids
AppInit modules (Chromium's child processes, Store apps) are not reached; browsers use
--use-angle=vulkan instead.

The folder given with -Source holds this layout (the OpenGL DLLs come from venus/build/build.sh,
out and out32, the hook from the same build, the DXVK DLLs from a DXVK release):
  wbhook64.dll  wbhook32.dll
  x64\  opengl32.dll libgallium_wgl.dll dxgi.dll d3d11.dll d3d10core.dll d3d9.dll
  x86\  the same names, 32-bit
It is copied to -InstallDir (no spaces: AppInit_DLLs is a space-separated list), which only
Administrators and SYSTEM may change, because every program of every user loads the hook.

Settings in HKLM\SOFTWARE\WinBridge\Hook: Components (1 = OpenGL, 2 = Direct3D, 3 = both) and Deny
(file names, REG_MULTI_SZ). The registry holds what AppInit_DLLs, LoadAppInit_DLLs and
RequireSignedAppInit_DLLs were before, so -Uninstall puts them back. A program starts the hook with
the next launch; programs that are already running keep what they have.

AppInit_DLLs is ignored when Secure Boot is on: the script says so.

Run it from an elevated PowerShell. -Status only reports.
#>

[CmdletBinding()]
param (
    [string]$Source = ".",
    [string]$InstallDir = "C:\ProgramData\WinBridge\Hook",
    [ValidateSet("OpenGL", "Direct3D", "Both")]
    [string]$Components = "Both",
    [string[]]$Deny = @(),
    [switch]$Uninstall,
    [switch]$Status
)

$ErrorActionPreference = "Stop"

$Settings = "HKLM:\SOFTWARE\WinBridge\Hook"
$Windows64 = "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Windows"
$Windows32 = "HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows NT\CurrentVersion\Windows"
$Dlls = @("opengl32.dll", "libgallium_wgl.dll", "dxgi.dll", "d3d11.dll", "d3d10core.dll", "d3d9.dll")

function Get-List([string]$Key) {
    $value = (Get-ItemProperty -Path $Key -Name AppInit_DLLs -ErrorAction SilentlyContinue).AppInit_DLLs
    if ($null -eq $value) { return @() }
    return @($value -split "[ ,]+" | Where-Object { $_ -ne "" })
}

# Remembers a value once, before the first change, as "<name>" ("-" when it did not exist).
function Save-Previous([string]$Key, [string]$Name, [string]$Store) {
    if ($null -ne (Get-ItemProperty -Path $Settings -Name $Store -ErrorAction SilentlyContinue).$Store) { return }
    $old = (Get-ItemProperty -Path $Key -Name $Name -ErrorAction SilentlyContinue).$Name
    New-ItemProperty -Path $Settings -Name $Store -Value $(if ($null -eq $old) { "-" } else { [string]$old }) -PropertyType String -Force | Out-Null
}

function Restore-Previous([string]$Key, [string]$Name, [string]$Store, [string]$Type) {
    $old = (Get-ItemProperty -Path $Settings -Name $Store -ErrorAction SilentlyContinue).$Store
    if ($null -eq $old) { return }
    if ($old -eq "-") {
        Remove-ItemProperty -Path $Key -Name $Name -ErrorAction SilentlyContinue
    } elseif ($Type -eq "DWord") {
        New-ItemProperty -Path $Key -Name $Name -Value ([int]$old) -PropertyType DWord -Force | Out-Null
    } else {
        New-ItemProperty -Path $Key -Name $Name -Value $old -PropertyType String -Force | Out-Null
    }
}

function Enable-Hook([string]$Key, [string]$Prefix, [string]$Dll) {
    Save-Previous $Key "AppInit_DLLs" "$($Prefix)PreviousAppInit"
    Save-Previous $Key "LoadAppInit_DLLs" "$($Prefix)PreviousLoad"
    Save-Previous $Key "RequireSignedAppInit_DLLs" "$($Prefix)PreviousSigned"
    $list = @(Get-List $Key | Where-Object { $_ -ne $Dll }) + $Dll
    New-ItemProperty -Path $Key -Name AppInit_DLLs -Value ($list -join " ") -PropertyType String -Force | Out-Null
    New-ItemProperty -Path $Key -Name LoadAppInit_DLLs -Value 1 -PropertyType DWord -Force | Out-Null
    New-ItemProperty -Path $Key -Name RequireSignedAppInit_DLLs -Value 0 -PropertyType DWord -Force | Out-Null
}

function Disable-Hook([string]$Key, [string]$Prefix, [string]$Dll) {
    $rest = @(Get-List $Key | Where-Object { $_ -ne $Dll })
    if ($rest.Count -gt 0) {
        # Another module still needs AppInit: only our entry goes, the switches stay on.
        New-ItemProperty -Path $Key -Name AppInit_DLLs -Value ($rest -join " ") -PropertyType String -Force | Out-Null
        return
    }
    Restore-Previous $Key "AppInit_DLLs" "$($Prefix)PreviousAppInit" "String"
    Restore-Previous $Key "LoadAppInit_DLLs" "$($Prefix)PreviousLoad" "DWord"
    Restore-Previous $Key "RequireSignedAppInit_DLLs" "$($Prefix)PreviousSigned" "DWord"
}

function Show-Status {
    Write-Host "=== WinBridge graphics hook ===" -ForegroundColor Cyan
    foreach ($pair in @(@("64-bit", $Windows64), @("32-bit", $Windows32))) {
        $item = Get-ItemProperty -Path $pair[1] -ErrorAction SilentlyContinue
        Write-Host ("{0}: AppInit_DLLs='{1}' LoadAppInit_DLLs={2} RequireSignedAppInit_DLLs={3}" -f $pair[0], $item.AppInit_DLLs, $item.LoadAppInit_DLLs, $item.RequireSignedAppInit_DLLs)
    }
    $set = Get-ItemProperty -Path $Settings -ErrorAction SilentlyContinue
    Write-Host ("Components={0} Deny='{1}'" -f $set.Components, ($set.Deny -join ", "))
    Write-Host ("Files in {0}: {1}" -f $InstallDir, $(if (Test-Path $InstallDir) { (Get-ChildItem $InstallDir -Recurse -File | ForEach-Object { $_.FullName.Substring($InstallDir.Length + 1) }) -join ", " } else { "none" }))
    try {
        Write-Host ("Secure Boot: {0}" -f (Confirm-SecureBootUEFI))
    } catch {
        Write-Host "Secure Boot: not enabled (the firmware has no Secure Boot variable)"
    }
}

if ($Status) {
    Show-Status
    exit 0
}

$Hook64 = Join-Path $InstallDir "wbhook64.dll"
$Hook32 = Join-Path $InstallDir "wbhook32.dll"

if ($Uninstall) {
    Write-Host "=== Removing the WinBridge graphics hook ===" -ForegroundColor Cyan
    Disable-Hook $Windows64 "" $Hook64
    Disable-Hook $Windows32 "32" $Hook32
    Remove-Item -Path $Settings -Recurse -Force -ErrorAction SilentlyContinue
    # A running program keeps the hook loaded, and with it the files: say so instead of failing.
    Remove-Item -Path $InstallDir -Recurse -Force -ErrorAction SilentlyContinue
    if (Test-Path $InstallDir) {
        Write-Warning "Some files in $InstallDir are in use by running programs; delete the folder after they end."
    }
    exit 0
}

if ($InstallDir -match "\s") {
    Write-Error "InstallDir must not contain spaces (AppInit_DLLs is a space-separated list)."
    exit 1
}
foreach ($arch in @("x64", "x86")) {
    foreach ($dll in $Dlls) {
        if (-not (Test-Path (Join-Path $Source "$arch\$dll"))) {
            Write-Error "Missing $arch\$dll in '$Source'. See Get-Help for the layout."
            exit 1
        }
    }
}
foreach ($hook in @("wbhook64.dll", "wbhook32.dll")) {
    if (-not (Test-Path (Join-Path $Source $hook))) {
        Write-Error "Missing $hook in '$Source'."
        exit 1
    }
}

Write-Host "=== Installing the WinBridge graphics hook ===" -ForegroundColor Cyan
New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
# Only SYSTEM and Administrators may write: every program of every user loads these files.
& icacls.exe $InstallDir /inheritance:r /grant:r "*S-1-5-18:(OI)(CI)F" "*S-1-5-32-544:(OI)(CI)F" "*S-1-5-32-545:(OI)(CI)RX" | Out-Null
if ($LASTEXITCODE -ne 0) {
    Write-Error "icacls failed on $InstallDir"
    exit 1
}
foreach ($arch in @("x64", "x86")) {
    New-Item -ItemType Directory -Path (Join-Path $InstallDir $arch) -Force | Out-Null
    foreach ($dll in $Dlls) {
        Copy-Item -Path (Join-Path $Source "$arch\$dll") -Destination (Join-Path $InstallDir "$arch\$dll") -Force
    }
}
Copy-Item -Path (Join-Path $Source "wbhook64.dll") -Destination $Hook64 -Force
Copy-Item -Path (Join-Path $Source "wbhook32.dll") -Destination $Hook32 -Force

New-Item -Path $Settings -Force | Out-Null
$mask = switch ($Components) { "OpenGL" { 1 } "Direct3D" { 2 } default { 3 } }
New-ItemProperty -Path $Settings -Name Components -Value $mask -PropertyType DWord -Force | Out-Null
New-ItemProperty -Path $Settings -Name Deny -Value ([string[]]$Deny) -PropertyType MultiString -Force | Out-Null

Enable-Hook $Windows64 "" $Hook64
Enable-Hook $Windows32 "32" $Hook32

Show-Status
try {
    if (Confirm-SecureBootUEFI) {
        Write-Warning "Secure Boot is on: Windows ignores AppInit_DLLs, so the hook does nothing."
    }
} catch {
}
Write-Host "=== Done. Programs started from now on get the hook. ===" -ForegroundColor Green
