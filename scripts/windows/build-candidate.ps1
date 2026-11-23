[CmdletBinding()]
param(
    [Parameter(Mandatory)][string] $MonitorOrigin,
    [Parameter(Mandatory)][string] $OutputDirectory
)

$ErrorActionPreference = 'Stop'
$target = 'x86_64-pc-windows-msvc'
$tauriCli = '2.12.5'
$origin = $null
if ($MonitorOrigin -cnotmatch '^https://(?:\[[0-9A-Fa-f:.]+\]|[A-Za-z0-9.-]+)(?::[0-9]{1,5})?/?$' -or
    -not [Uri]::TryCreate($MonitorOrigin, [UriKind]::Absolute, [ref] $origin) -or
    $origin.HostNameType -eq [UriHostNameType]::Unknown -or $origin.Port -lt 1) {
    throw 'Provide the monitor HTTPS origin with an optional port. Use an ASCII host and omit the path, query, fragment, and credentials.'
}
if (-not $IsWindows -or (rustc -Vv) -notcontains "host: $target") {
    throw "The candidate must be built on Windows with the $target host toolchain."
}

$repo = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$commit = git -C $repo rev-parse HEAD
if ($LASTEXITCODE -ne 0) { throw 'Cannot identify the source commit.' }
$changes = git -C $repo status --porcelain --untracked-files=all
if ($LASTEXITCODE -ne 0 -or $changes -or $env:TAURI_CONFIG) {
    throw 'Build from a clean Git checkout with no TAURI_CONFIG override so candidate.json identifies the exact source.'
}
$output = (New-Item -ItemType Directory -Force -Path $OutputDirectory).FullName
$metadata = cargo metadata --locked --no-deps --format-version 1 --manifest-path (Join-Path $repo 'Cargo.toml')
if ($LASTEXITCODE -ne 0) { throw 'Cargo metadata failed.' }
$targetDirectory = ($metadata | ConvertFrom-Json).target_directory
$app = Join-Path $repo 'crates/cvbbmso-client'
$env:CVBBMSO_SERVER_ORIGIN = $origin.GetLeftPart([UriPartial]::Authority)
$env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS = '-C target-feature=+crt-static'
Push-Location $app
try {
    npm exec --yes --package "@tauri-apps/cli@$tauriCli" -- tauri build --ci --bundles msi --target $target -- --locked
    if ($LASTEXITCODE -ne 0) { throw "The Tauri build failed with exit code $LASTEXITCODE." }
} finally {
    Pop-Location
}
$currentCommit = git -C $repo rev-parse HEAD
if ($LASTEXITCODE -ne 0 -or $currentCommit -ne $commit) { throw 'The source commit changed during the build.' }
$changes = git -C $repo status --porcelain --untracked-files=all
if ($LASTEXITCODE -ne 0 -or $changes) { throw 'The source checkout changed during the build.' }

$version = ($metadata | ConvertFrom-Json).packages.Where({ $_.name -eq 'cvbbmso-client' })[0].version
$name = "CVBBMSO_${version}_x64_en-US.msi"
$built = Join-Path $targetDirectory "$target/release/bundle/msi/$name"
if (-not (Test-Path -LiteralPath $built -PathType Leaf)) { throw "Expected installer missing: $built" }
$installer = Join-Path $output $name
Copy-Item -LiteralPath $built -Destination $installer
$digest = (Get-FileHash -Algorithm SHA256 -LiteralPath $installer).Hash.ToLowerInvariant()
$signature = (Get-AuthenticodeSignature -LiteralPath $installer).Status.ToString()
[ordered]@{
    commit = $commit
    version = $version
    target = $target
    tauri_cli = $tauriCli
    monitor_origin = $env:CVBBMSO_SERVER_ORIGIN
    file = $name
    sha256 = $digest
    signature = $signature
} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $output 'candidate.json') -Encoding utf8
Write-Host "Windows candidate: $installer"
