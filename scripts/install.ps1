$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$manifest = Get-Content (Join-Path $root 'herdr-plugin.toml') -Raw
$version = [regex]::Match($manifest, '(?m)^version\s*=\s*"([^"]+)"').Groups[1].Value
if (!$version) { throw 'Missing manifest version' }
# Only x64 Windows has a release asset; other architectures use the local toolchain.
$arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
$asset = 'herdr-respan-x86_64-pc-windows-msvc.exe'
$url = "https://github.com/jaredlyon/herdr-respan-adapter/releases/download/v$version"
$bin = Join-Path $root 'bin'
New-Item -ItemType Directory -Force $bin | Out-Null
$tmp = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory $tmp | Out-Null
try {
    $downloaded = $false
    if ($arch -eq 'AMD64') {
        try {
            [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
            Invoke-WebRequest -UseBasicParsing "$url/$asset" -OutFile (Join-Path $tmp $asset)
            Invoke-WebRequest -UseBasicParsing "$url/SHA256SUMS" -OutFile (Join-Path $tmp 'SHA256SUMS')
            $downloaded = $true
        } catch { Write-Warning 'Prebuilt release unavailable; using Cargo fallback.' }
    }
    $new = Join-Path $bin 'herdr-respan.exe.new'
    if ($downloaded) {
        $matches = @(Get-Content (Join-Path $tmp 'SHA256SUMS') | Where-Object { $_ -match ('^[a-fA-F0-9]{64}\s+' + [regex]::Escape($asset) + '$') })
        if ($matches.Count -ne 1) { throw 'Missing or ambiguous release checksum' }
        $expected = ($matches[0] -split '\s+')[0]
        $actual = (Get-FileHash (Join-Path $tmp $asset) -Algorithm SHA256).Hash
        if ($actual -ne $expected) { throw 'Release SHA256 mismatch; refusing executable' }
        Copy-Item (Join-Path $tmp $asset) $new -Force
    } else {
        $cargo = Get-Command cargo -ErrorAction SilentlyContinue
        if (!$cargo) {
            $fallback = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
            if (!(Test-Path $fallback)) { throw 'Install Rust/Cargo or use a supported prebuilt release.' }
            $cargoPath = $fallback
        } else { $cargoPath = $cargo.Source }
        & $cargoPath build --release --locked
        if ($LASTEXITCODE -ne 0) { throw 'Cargo release build failed' }
        Copy-Item (Join-Path $root 'target\release\herdr-respan.exe') $new -Force
    }
    Move-Item $new (Join-Path $bin 'herdr-respan.exe') -Force
} finally { Remove-Item $tmp -Recurse -Force }
