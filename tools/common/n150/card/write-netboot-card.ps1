# Writes the Ferrix netboot stick image onto the 32 GB microSD card in the
# USB reader, then reads it back. Run from an elevated PowerShell with
# -Erase ERASE as the confirmation.
param([string]$Erase = '')
$ErrorActionPreference = 'Stop'
$img = 'G:\GameLab\iso\ferrix\ferrix-netboot-stick.img'
$want = '1926c3e23561756c8a85ca18e91f9984103cc2d0f8ddc4855cc86f859f9e7355'

$d = Get-Disk | Where-Object { $_.BusType -eq 'USB' -and $_.SerialNumber -eq '000000002964' -and $_.Size -eq 31914983424 }
if (@($d).Count -ne 1) { throw "expected exactly one 31914983424-byte USB card with serial 000000002964, found $(@($d).Count)" }
if ($d.IsBoot -or $d.IsSystem) { throw 'refusing: that disk is a boot or system disk' }
if ((Get-FileHash $img -Algorithm SHA256).Hash -ne $want) { throw 'image checksum mismatch' }
$n = $d.Number
Write-Host "Card: Disk $n, $($d.FriendlyName), serial $($d.SerialNumber), $($d.Size) bytes"
if ($Erase -cne 'ERASE') { throw "not confirmed: pass -Erase ERASE to overwrite Disk $n (got '$Erase')" }
Write-Host "Erasing and writing Disk $n ..."

# Removable media cannot be set offline; a cleared card has no volumes to hold it.
if ($d.PartitionStyle -ne 'RAW') { Clear-Disk -Number $n -RemoveData -RemoveOEM -Confirm:$false }
$bytes = [IO.File]::ReadAllBytes($img)
$dev = "\\.\PhysicalDrive$n"
$s = New-Object IO.FileStream($dev, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::ReadWrite)
$s.Write($bytes, 0, $bytes.Length); $s.Flush(); $s.Close()

$s = New-Object IO.FileStream($dev, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::ReadWrite)
$back = New-Object byte[] $bytes.Length
$got = 0
while ($got -lt $back.Length) { $r = $s.Read($back, $got, $back.Length - $got); if ($r -le 0) { break }; $got += $r }
$s.Close()
$h = [BitConverter]::ToString([Security.Cryptography.SHA256]::Create().ComputeHash($back, 0, $got)).Replace('-', '').ToLower()
Update-Disk -Number $n
if ($h -eq $want) { Write-Host "OK: read back $got bytes, sha256 matches. Eject the card." -ForegroundColor Green }
else { Write-Host "MISMATCH: read back $got bytes, sha256 $h" -ForegroundColor Red }
