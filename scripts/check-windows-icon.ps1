# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Fabian Schmieder

param(
    [Parameter(Mandatory = $true)]
    [string]$Executable,
    [int]$IconId = 1
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$native = @'
using System;
using System.Runtime.InteropServices;

public static class DevknxResources {
    [DllImport("kernel32.dll", EntryPoint = "LoadLibraryExW", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern IntPtr LoadLibraryEx(string path, IntPtr file, uint flags);

    [DllImport("kernel32.dll", EntryPoint = "FindResourceW", SetLastError = true)]
    public static extern IntPtr FindResource(IntPtr module, IntPtr name, IntPtr kind);

    [DllImport("kernel32.dll", EntryPoint = "SizeofResource", SetLastError = true)]
    public static extern uint SizeofResource(IntPtr module, IntPtr resource);

    [DllImport("kernel32.dll", EntryPoint = "FreeLibrary", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool FreeLibrary(IntPtr module);
}
'@
Add-Type -TypeDefinition $native

$path = (Resolve-Path -LiteralPath $Executable).ProviderPath
$loadAsDataFile = 0x00000002
$groupIconType = 14
$module = [DevknxResources]::LoadLibraryEx($path, [IntPtr]::Zero, $loadAsDataFile)
if ($module -eq [IntPtr]::Zero) {
    throw "Could not load PE resources from $path"
}

$missing = $false
try {
    $icon = [DevknxResources]::FindResource(
        $module, [IntPtr]$IconId, [IntPtr]$groupIconType
    )
    if ($icon -eq [IntPtr]::Zero) {
        $missing = $true
    } elseif ([DevknxResources]::SizeofResource($module, $icon) -lt 20) {
        throw "Group icon $IconId has an invalid resource size"
    }
} finally {
    if (-not [DevknxResources]::FreeLibrary($module)) {
        throw 'Could not release PE resources'
    }
}

if ($missing) {
    # Distinct from another script or loader error; CI expects this exact exit
    # for the deliberately absent icon ID in its counter-probe.
    Write-Output "Group icon $IconId was not found in $path"
    exit 23
}

Write-Output "Group icon $IconId is embedded in $path"
