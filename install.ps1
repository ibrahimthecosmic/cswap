# Install cswap on Windows. Usage:
#   irm https://raw.githubusercontent.com/ibrahimthecosmic/cswap/main/install.ps1 | iex
#
# Parameters can be set beforehand as $env:CSWAP_INSTALL_DIR / $env:CSWAP_VERSION.

$ErrorActionPreference = 'Stop'

$Repo    = 'ibrahimthecosmic/cswap'
$Version = if ($env:CSWAP_VERSION) { $env:CSWAP_VERSION } else { 'latest' }
$Dir     = if ($env:CSWAP_INSTALL_DIR) { $env:CSWAP_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\cswap' }

if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne 'X64') {
    throw "no Windows binary is published for $([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) — build from source: cargo install --git https://github.com/$Repo"
}

$Asset = 'cswap-windows-x86_64.exe'
$Url = if ($Version -eq 'latest') {
    "https://github.com/$Repo/releases/latest/download/$Asset"
} else {
    "https://github.com/$Repo/releases/download/$Version/$Asset"
}

Write-Host "Downloading $Asset ($Version)..."
$Temp = Join-Path ([System.IO.Path]::GetTempPath()) "cswap-$([guid]::NewGuid()).exe"
try {
    # TLS 1.2 for Windows PowerShell 5.1, which does not negotiate it by default.
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    Invoke-WebRequest -Uri $Url -OutFile $Temp -UseBasicParsing
} catch {
    throw "download failed — does the release exist? $Url"
}

# Refuse an HTML error page renamed to look like a binary.
$Magic = [System.IO.File]::ReadAllBytes($Temp)[0..1]
if ($Magic[0] -ne 0x4D -or $Magic[1] -ne 0x5A) {
    Remove-Item $Temp -Force
    throw 'downloaded file is not a Windows executable'
}

New-Item -ItemType Directory -Force -Path $Dir | Out-Null
$Target = Join-Path $Dir 'cswap.exe'
Move-Item -Force $Temp $Target

Write-Host "Installed $Target"
& $Target --version

# Put it on the user PATH for future sessions, and this one.
$UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if ($UserPath -notlike "*$Dir*") {
    [Environment]::SetEnvironmentVariable('Path', "$UserPath;$Dir", 'User')
    Write-Host "Added $Dir to your user PATH — open a new terminal to pick it up."
}
$env:Path = "$env:Path;$Dir"
