param(
  [string]$Root = "$HOME\.local"
)

$ErrorActionPreference = "Stop"
$repoDir = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$llmonHome = if ($env:LLMON_HOME -and $env:LLMON_HOME.Trim()) {
  $env:LLMON_HOME
} else {
  Join-Path $HOME ".llmon"
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
  throw "cargo not found in PATH. Install Rust first: https://rustup.rs"
}

if (Test-Path -LiteralPath $llmonHome) {
  $homeItem = Get-Item -LiteralPath $llmonHome -Force
  if ($homeItem.Attributes -band [IO.FileAttributes]::ReparsePoint) {
    throw "Refusing to use LLMON_HOME ($llmonHome): symlink/reparse point is not allowed."
  }
  if (-not $homeItem.PSIsContainer) {
    throw "Refusing to use LLMON_HOME ($llmonHome): expected a directory."
  }
} else {
  New-Item -ItemType Directory -Path $llmonHome | Out-Null
}

cargo install --path $repoDir --locked --force --root $Root

$binDir = Join-Path $Root "bin"
Write-Host "Installed llmon to $(Join-Path $binDir 'llmon.exe')"
Write-Host "Prepared LLMON_HOME at $llmonHome"

$pathEntries = $env:PATH -split ";"
if (-not ($pathEntries -contains $binDir)) {
  Write-Host "Add to PATH: $binDir"
}
