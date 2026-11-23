[CmdletBinding()]
param([Parameter(Mandatory)][string] $CandidateDirectory)

$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted') {
    throw 'This installation test runs only on disposable GitHub-hosted runners.'
}
$manifest = Get-Content -Raw -LiteralPath (Join-Path $CandidateDirectory 'candidate.json') | ConvertFrom-Json
$msi = (Resolve-Path -LiteralPath (Join-Path $CandidateDirectory $manifest.file)).Path
if ((Get-FileHash -Algorithm SHA256 -LiteralPath $msi).Hash -ne $manifest.sha256) {
    throw 'The installer hash does not match candidate.json.'
}
$executable = Join-Path $env:ProgramFiles 'CVBBMSO/cvbbmso-client.exe'
$data = Join-Path $env:LOCALAPPDATA 'edu.buaa.accoding.cvbbmso'
$fixture = Join-Path $data "exams/$([Guid]::NewGuid())/42/recording"
if (Test-Path -LiteralPath $executable) { throw 'The runner already has a CVBBMSO installation.' }
$fixturePath = Join-Path (New-Item -ItemType Directory -Force -Path $fixture).FullName 'sentinel.txt'
$sentinel = [Guid]::NewGuid().ToString()
Set-Content -LiteralPath $fixturePath -Value $sentinel -NoNewline

function Invoke-Installer([string] $Mode, [string] $LogName) {
    $log = Join-Path $CandidateDirectory $LogName
    # Silent installation neither launches CVBBMSO nor starts its egress guard.
    $process = Start-Process -FilePath "$env:SystemRoot/System32/msiexec.exe" `
        -ArgumentList "$Mode `"$msi`" /qn /norestart /L*v `"$log`"" -Wait -PassThru
    if ($process.ExitCode -ne 0) { throw "msiexec $Mode returned $($process.ExitCode). See $LogName." }
}

Invoke-Installer '/i' 'install.log'
try {
    if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) {
        throw 'The per-machine installation did not install the client in Program Files.'
    }
    $tool = Get-Command dumpbin.exe -ErrorAction SilentlyContinue
    if (-not $tool) {
        $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
        $visualStudio = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        $tool = Get-ChildItem -Path "$visualStudio/VC/Tools/MSVC/*/bin/Hostx64/x64/dumpbin.exe" -ErrorAction Stop |
            Sort-Object LastWriteTime -Descending | Select-Object -First 1
    }
    if (-not $tool) { throw 'Cannot locate MSVC dumpbin.' }
    $dumpbin = if ($tool -is [System.IO.FileInfo]) { $tool.FullName } else { $tool.Source }
    $imports = & $dumpbin /headers /dependents $executable
    if ($LASTEXITCODE -ne 0) { throw 'Failed to inspect the installed executable.' }
    if (-not ($imports -match '8664 machine \(x64\)')) { throw 'The installed executable is not x64.' }
    if ($imports -match '(?i)\b(?:vcruntime|msvcp|concrt)\w*\.dll\b') {
        throw 'The installed executable imports a dynamic Visual C++ runtime.'
    }
    if ((Get-Content -Raw -LiteralPath $fixturePath) -ne $sentinel) {
        throw 'Installation changed the existing recording directory.'
    }
} finally {
    Invoke-Installer '/x' 'uninstall.log'
}

if (Test-Path -LiteralPath $executable) { throw 'The uninstaller did not remove the client.' }
if ((Get-Content -Raw -LiteralPath $fixturePath) -ne $sentinel) {
    throw 'Uninstall did not preserve the existing recording directory.'
}
Write-Host 'Verified per-machine x64 installation, static Visual C++ runtime, and preservation of recording data on uninstall.'
