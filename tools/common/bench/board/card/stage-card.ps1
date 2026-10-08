<#
.SYNOPSIS
  Put the board matrix's files on the DK1 card's bootfs, after backing up
  what is there, and check every byte that lands (docs/BOARD-BENCH.md, B4).

.DESCRIPTION
  -Stage is a directory laid out exactly as bootfs should gain it (bench/,
  and EFI/ and FERRIX/ when a Ferrix image is to be the active one), with a
  SHA256SUMS file of `<hash>  <path>` lines covering every file in it.

  Without -Apply it only reports: the drive it found, the backup it would
  take, the space needed and free. With -Apply it:
    1. refuses unless the drive is FAT32, labelled bootfs, 120 to 140 MiB
       (prepare-card-v2.sh's partition 4) -- so a wrong drive letter is
       refused before anything is read or written;
    2. copies the whole volume to a new directory under -Backup, with its
       own SHA256SUMS, and verifies the copy;
    3. with -ClearFerrix, deletes FERRIX/INITRD.IMG, KERNEL.ELF and
       DEFAULTS.TXT from the card (they are in the backup) to make room;
    4. copies the stage's files and reads each back against SHA256SUMS.

  -Restore <backup dir> puts a backup back: removes bench/ and the FERRIX/
  files the stage wrote, copies the backup's files, and verifies them.

  Nothing is formatted or repartitioned; only files on bootfs change.
#>
param(
    [Parameter(Mandatory = $true)][string]$Drive,
    [string]$Stage,
    [string]$Backup,
    [string]$Restore,
    [switch]$ClearFerrix,
    [switch]$Apply
)

$ErrorActionPreference = "Stop"

function Fail([string]$why) { throw "stage-card: $why" }

$letter = $Drive.TrimEnd(":", "\")
if ($letter.Length -ne 1) { Fail "-Drive is one letter, e.g. E: (got $Drive)" }
$root = "${letter}:\"
$vol = Get-Volume -DriveLetter $letter -ErrorAction SilentlyContinue
if ($null -eq $vol) { Fail "no volume at ${letter}:" }
$mib = [math]::Round($vol.Size / 1MB, 1)
"drive ${letter}: label '$($vol.FileSystemLabel)' $($vol.FileSystem) $mib MiB, $([math]::Round($vol.SizeRemaining / 1MB, 1)) MiB free, type $($vol.DriveType)"
if ($vol.FileSystem -ne "FAT32") { Fail "${letter}: is $($vol.FileSystem), not FAT32" }
if ($vol.FileSystemLabel -ne "bootfs") { Fail "${letter}: is labelled '$($vol.FileSystemLabel)', not bootfs" }
if ($mib -lt 120 -or $mib -gt 140) { Fail "${letter}: is $mib MiB, not the 128 MiB bootfs" }

function Read-Sums([string]$dir) {
    $sums = [ordered]@{}
    $file = Join-Path $dir "SHA256SUMS"
    if (-not (Test-Path -LiteralPath $file)) { Fail "$dir has no SHA256SUMS" }
    foreach ($line in Get-Content -LiteralPath $file) {
        if ($line -match '^([0-9a-fA-F]{64})\s+\*?(.+)$') { $sums[$Matches[2].Replace("\", "/")] = $Matches[1].ToLower() }
    }
    return $sums
}

function Hash([string]$path) { (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToLower() }

function Files-Under([string]$dir) {
    Get-ChildItem -LiteralPath $dir -Recurse -File -Force |
        ForEach-Object { $_.FullName.Substring($dir.TrimEnd("\").Length + 1).Replace("\", "/") }
}

function Copy-Verified([string]$from, [string]$to, $sums) {
    foreach ($rel in $sums.Keys) {
        $src = Join-Path $from $rel
        $dst = Join-Path $to $rel
        if (-not (Test-Path -LiteralPath $src)) { Fail "SHA256SUMS names $rel, which $from does not have" }
        if ((Hash $src) -ne $sums[$rel]) { Fail "$src does not match its SHA256SUMS line" }
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $dst) | Out-Null
        Copy-Item -LiteralPath $src -Destination $dst -Force
    }
    # Read back after every copy is done, so the check reads the card, not a
    # cache of the write just made as far as Windows lets us.
    foreach ($rel in $sums.Keys) {
        $dst = Join-Path $to $rel
        if ((Hash $dst) -ne $sums[$rel]) { Fail "$dst does not read back as written" }
    }
    "  $($sums.Count) files copied to $to and read back"
}

function Take-Backup {
    if (-not $Backup) { Fail "-Backup <dir> is required: the card's files are copied off before any write" }
    $stamp = (Get-Date).ToString("yyyyMMdd-HHmmss")
    $dir = Join-Path $Backup "bootfs-$stamp"
    if (Test-Path -LiteralPath $dir) { Fail "$dir exists" }
    New-Item -ItemType Directory -Path $dir | Out-Null
    $lines = @()
    foreach ($rel in (Files-Under $root)) {
        $src = Join-Path $root $rel
        $dst = Join-Path $dir $rel
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $dst) | Out-Null
        Copy-Item -LiteralPath $src -Destination $dst
        $h = Hash $src
        if ((Hash $dst) -ne $h) { Fail "backup of $rel does not match the card" }
        $lines += "$h  $rel"
    }
    $lines | Set-Content -Encoding ascii (Join-Path $dir "SHA256SUMS")
    Write-Host "  backup: $($lines.Count) files in $dir"
    return $dir
}

if ($Restore) {
    $sums = Read-Sums $Restore
    "restore from $Restore ($($sums.Count) files)"
    if (-not $Apply) { "  (report only; -Apply to write)"; return }
    $null = Take-Backup
    $bench = Join-Path $root "bench"
    if (Test-Path -LiteralPath $bench) { Remove-Item -LiteralPath $bench -Recurse -Force }
    foreach ($f in "INITRD.IMG", "KERNEL.ELF", "DEFAULTS.TXT", "CMDLINE.TXT") {
        $p = Join-Path $root "FERRIX\$f"
        if ((Test-Path -LiteralPath $p) -and -not $sums.Contains("FERRIX/$f")) { Remove-Item -LiteralPath $p -Force }
    }
    Copy-Verified $Restore $root $sums
    return
}

if (-not $Stage) { Fail "-Stage <dir> or -Restore <dir>" }
$sums = Read-Sums $Stage
$need = 0
$freed = 0
foreach ($rel in $sums.Keys) {
    $need += (Get-Item -LiteralPath (Join-Path $Stage $rel)).Length
    $old = Join-Path $root $rel
    if (Test-Path -LiteralPath $old) { $freed += (Get-Item -LiteralPath $old).Length }
}
if ($ClearFerrix) {
    foreach ($f in "INITRD.IMG", "KERNEL.ELF", "DEFAULTS.TXT") {
        $p = Join-Path $root "FERRIX\$f"
        if ((Test-Path -LiteralPath $p) -and -not $sums.Contains("FERRIX/$f")) { $freed += (Get-Item -LiteralPath $p).Length }
    }
}
# FAT32 clusters round every file up; leave 4 MiB besides.
$room = $vol.SizeRemaining + $freed - 4MB
"stage $Stage`: $($sums.Count) files, $([math]::Round($need / 1MB, 1)) MiB; room after replacing $([math]::Round($room / 1MB, 1)) MiB"
if ($need -gt $room) { Fail "not enough room on bootfs$(if (-not $ClearFerrix) { '; -ClearFerrix frees the desktop image (it is backed up first)' })" }
if (-not $Apply) { "  (report only; -Apply to back up and write)"; return }

$null = Take-Backup
if ($ClearFerrix) {
    foreach ($f in "INITRD.IMG", "KERNEL.ELF", "DEFAULTS.TXT") {
        $p = Join-Path $root "FERRIX\$f"
        if (Test-Path -LiteralPath $p) { Remove-Item -LiteralPath $p -Force }
    }
}
Copy-Verified $Stage $root $sums
