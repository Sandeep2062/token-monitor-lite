# Lightweight byte transfer only; all Python, Cargo and processing run on the VM.
param(
    [ValidateSet('test', 'format', 'verify', 'desktop-check', 'desktop-verify', 'credential-probe', 'credential-lifecycle', 'linux-gui', 'linux-resource', 'linux-package', 'windows-cross')][string]$Mode = 'test',
    [string]$Test = 'core'
)
$ErrorActionPreference = 'Stop'
$Files = @('Cargo.toml', 'build.rs', 'src/lib.rs', 'tests/core.rs', 'LICENSE')
foreach ($Optional in @('Cargo.lock', 'src/catalog.rs', 'src/accounting.rs', 'assets/catalog.json')) {
    if (Test-Path -LiteralPath $Optional) { $Files += $Optional }
}
foreach ($AcceptanceFile in @('scripts/generate-acceptance-fixture.py', 'scripts/run-linux-acceptance.py', 'scripts/run-linux-resource-acceptance.py', 'scripts/run-linux-credential-session.py', 'scripts/build-linux-packages.py', 'scripts/run-linux-package-acceptance.py', 'scripts/acceptance-evidence.schema.json')) {
    if (Test-Path -LiteralPath $AcceptanceFile) { $Files += $AcceptanceFile }
}
$Files += @(Get-ChildItem -Path 'src','tests' -Recurse -File | ForEach-Object {
    $_.FullName.Substring($PWD.Path.Length + 1).Replace('\', '/')
} | Where-Object { $_ -notin $Files })
if (Test-Path -LiteralPath 'ui') {
    $Files += @(Get-ChildItem -Path 'ui' -Recurse -File | ForEach-Object {
        $_.FullName.Substring($PWD.Path.Length + 1).Replace('\', '/')
    } | Where-Object { $_ -notin $Files })
}
if (Test-Path -LiteralPath 'packaging') {
    $Files += @(Get-ChildItem -Path 'packaging' -Recurse -File | ForEach-Object {
        $_.FullName.Substring($PWD.Path.Length + 1).Replace('\', '/')
    } | Where-Object { $_ -notin $Files })
}
$Entries = foreach ($File in $Files) {
    @{ path = $File; sha256 = (Get-FileHash -LiteralPath $File -Algorithm SHA256).Hash.ToLower(); bytes = [Convert]::ToBase64String([IO.File]::ReadAllBytes((Join-Path $PWD $File))) }
}
foreach ($Pair in @(
    @('token-monitor/src/shared/clientCatalog.js', 'reference/clientCatalog.js'),
    @('token-monitor/src/shared/limits/providers.js', 'reference/providers.js'),
    @('scripts/check-catalog.cjs', 'scripts/check-catalog.cjs')
)) {
    $Entries += @{ path = $Pair[1]; sha256 = (Get-FileHash -LiteralPath $Pair[0] -Algorithm SHA256).Hash.ToLower(); bytes = [Convert]::ToBase64String([IO.File]::ReadAllBytes((Join-Path $PWD $Pair[0]))) }
}
$Payload = @{ files = @($Entries); mode = $Mode; test = $Test } | ConvertTo-Json -Depth 5 -Compress
$Id = [Guid]::NewGuid().ToString('N')
$Remote = "/home/azureuser/.local/state/token-monitor-lite/transfer-$Id"
$LocalPayload = Join-Path "$env:LOCALAPPDATA\Temp\opencode" "core-payload-$Id.json"
[IO.File]::WriteAllText($LocalPayload, $Payload, [Text.UTF8Encoding]::new($false))
ssh -o BatchMode=yes pixvault-vm "mkdir -m 700 $Remote"
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
scp -q $LocalPayload './scripts/remote-core-runner.py' "pixvault-vm:$Remote/"
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
ssh -o BatchMode=yes pixvault-vm "python3 $Remote/remote-core-runner.py $Remote/core-payload-$Id.json"
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
