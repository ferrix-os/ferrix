# Writes an image onto the 32 GB microSD card in the USB reader, then reads
# it back and compares SHA-256. Run from an elevated PowerShell:
#   write-card.ps1 -Image <file> -Sha256 <hex> -Erase ERASE
#   write-card.ps1 -Image <file> -Sha256 <hex> -VerifyOnly   (read back only)
param([string]$Image, [string]$Sha256, [string]$Erase = '', [switch]$VerifyOnly)
$ErrorActionPreference = 'Stop'
$want = $Sha256.ToLower()

$d = Get-Disk | Where-Object { $_.BusType -eq 'USB' -and $_.SerialNumber -eq '000000002964' -and $_.Size -eq 31914983424 }
if (@($d).Count -ne 1) { throw "expected exactly one 31914983424-byte USB card with serial 000000002964, found $(@($d).Count)" }
if ($d.IsBoot -or $d.IsSystem) { throw 'refusing: that disk is a boot or system disk' }
$len = (Get-Item $Image).Length
if ($len -gt $d.Size) { throw 'image is larger than the card' }
Write-Host "Checking $Image ($len bytes) ..."
if ((Get-FileHash $Image -Algorithm SHA256).Hash.ToLower() -ne $want) { throw 'image checksum mismatch' }
$n = $d.Number
Write-Host "Card: Disk $n, $($d.FriendlyName), serial $($d.SerialNumber), $($d.Size) bytes"
$dev = "\\.\PhysicalDrive$n"
$chunk = 4MB
$buf = New-Object byte[] $chunk
if (-not $VerifyOnly) {
if ($Erase -cne 'ERASE') { throw "not confirmed: pass -Erase ERASE to overwrite Disk $n (got '$Erase')" }

# Removable media cannot be set offline; a cleared card has no volumes to hold it.
if ($d.PartitionStyle -ne 'RAW') { Clear-Disk -Number $n -RemoveData -RemoveOEM -Confirm:$false }

Write-Host "Writing Disk $n ..."
$in = [IO.File]::OpenRead($Image)
$out = New-Object IO.FileStream($dev, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::ReadWrite)
$done = 0
while (($r = $in.Read($buf, 0, $chunk)) -gt 0) {
    # Raw disk writes must be whole sectors; pad the last piece with zeros.
    $w = [int]([math]::Ceiling($r / 512) * 512)
    if ($w -ne $r) { [Array]::Clear($buf, $r, $w - $r) }
    $out.Write($buf, 0, $w)
    $done += $r
    Write-Progress -Activity 'Writing' -PercentComplete ([int](100 * $done / $len))
}
$out.Flush(); $out.Close(); $in.Close()
}

Write-Host 'Reading back ...'
$sha = [Security.Cryptography.SHA256]::Create()
$s = New-Object IO.FileStream($dev, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::ReadWrite)
$left = $len
while ($left -gt 0) {
    $r = $s.Read($buf, 0, $chunk)
    if ($r -le 0) { break }
    $take = [int][math]::Min([long]$r, [long]$left)
    [void]$sha.TransformBlock($buf, 0, $take, $null, 0)
    $left -= $take
    Write-Progress -Activity 'Verifying' -PercentComplete ([int](100 * ($len - $left) / $len))
}
[void]$sha.TransformFinalBlock($buf, 0, 0)
$s.Close()
$h = [BitConverter]::ToString($sha.Hash).Replace('-', '').ToLower()
Update-Disk -Number $n
if ($left -eq 0 -and $h -eq $want) { Write-Host "OK: read back $len bytes, sha256 matches. Eject the card." -ForegroundColor Green }
else { Write-Host "MISMATCH: $left bytes unread, sha256 $h" -ForegroundColor Red }
