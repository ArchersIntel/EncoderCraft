# Builds the static Linux x86_64 daemon (encraft-d) that the deploy wizard uploads.
# Pure Rust, so rust-lld is enough: no gcc, Docker or WSL needed. Works from Windows, macOS or Linux.
$ErrorActionPreference = 'Stop'
$target = 'x86_64-unknown-linux-musl'
rustup target add $target
$env:CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER = 'rust-lld'
$env:CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS = '-C linker-flavor=ld.lld'
Push-Location (Split-Path $PSScriptRoot -Parent)
try {
    cargo build --release -p encoder-daemon --target $target
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
    $targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { 'target' }
    $out = 'dist/encraft-d-linux-x86_64'
    Copy-Item "$targetDir/$target/release/encraft-d" $out -Force
    $hash = (Get-FileHash $out -Algorithm SHA256).Hash.ToLower()
    Write-Host "Wrote $out ($([math]::Round((Get-Item $out).Length/1MB,1)) MB) sha256 $hash"
} finally { Pop-Location }
