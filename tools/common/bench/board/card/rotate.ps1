<#
.SYNOPSIS
  Drive the DK1 through a rotation of kernels over its serial console, one
  boot at a time, and keep every byte (docs/BOARD-BENCH.md, B4).

.DESCRIPTION
  The board's U-Boot is stopped at each autoboot, given the named kernel's
  boot lines one prompt at a time, and the run is logged until the kernel's
  bench prints `board-bench end <prog>`. Every bench resets the board itself
  after that line (Linux `reboot -f`, seL4 through IWDG2, Ferrix
  `ferrix.onexit=reset`), so the next autoboot starts the next boot of the
  rotation with no hand at the board.

  One boot per line of the plan's order, `-Rounds` times over: with
  `-Order linux,sel4,ferrix -Rounds 5` the boots are A B C A B C ..., which is
  the turn-about the skill's section 7 asks for.

  Nothing persists outside the card. The plan's lines are the only commands
  sent; the saved U-Boot environment is never written (no `saveenv`).

  Written for Windows PowerShell 5.1 and System.IO.Ports, because the board's
  ST-LINK is COM8 on the Windows PC and that needs no Python package.

.PARAMETER Plan
  The plan file: `[name]` sections, each followed by the U-Boot lines that
  boot that kernel from the card, the last one being the boot command.
  `#` starts a comment line.

.PARAMETER Order
  Section names, comma separated, in rotation order.

.PARAMETER Out
  Directory for the logs. Created if missing; existing logs are never
  overwritten (a new numbered run directory is made inside it).

.EXAMPLE
  powershell -File rotate.ps1 -Plan card.plan -Order linux,sel4,ferrix -Rounds 5 -Out D:\dk1-runs
#>
param(
    [Parameter(Mandatory = $true)][string]$Plan,
    [Parameter(Mandatory = $true)][string]$Order,
    [int]$Rounds = 1,
    [Parameter(Mandatory = $true)][string]$Out,
    [string]$Port = "COM8",
    [int]$Baud = 115200,
    # Seconds a kernel may run from its boot command to `board-bench end`.
    [int]$BootTimeout = 900,
    # Seconds one U-Boot line may take to come back to the prompt (loads,
    # fatwrite of a 20 MB file).
    [int]$LineTimeout = 120,
    # Print what would be sent, open no port.
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"

$Prompt = "STM32MP> "
$Autoboot = "Hit any key to stop autoboot"
$EndMark = "board-bench end"
# A line from U-Boot that means the boot lines failed.
$UbootErrors = @("Unable to read file", "Unknown command", "Failed to load", "Bad Linux ARM zImage",
    "Wrong Image Format", "Error: ", "** No partition", "Could not find", "## Error", "Invalid partition")
# A line from a running kernel that means the boot is lost.
$KernelDeath = @("FERRIX-PANIC", "Kernel panic - not syncing", "KERNEL PANIC", "seL4 failed assertion")

function Read-Plan([string]$path) {
    $sections = [ordered]@{}
    $name = $null
    foreach ($raw in Get-Content -LiteralPath $path) {
        $line = $raw.Trim()
        if ($line -eq "" -or $line.StartsWith("#")) { continue }
        if ($line -match '^\[(.+)\]$') {
            $name = $Matches[1]
            $sections[$name] = New-Object System.Collections.Generic.List[string]
            continue
        }
        if ($null -eq $name) { throw "plan ${path}: a line before any [section]: $line" }
        $sections[$name].Add($line)
    }
    return $sections
}

$sections = Read-Plan $Plan
$names = $Order.Split(",") | ForEach-Object { $_.Trim() } | Where-Object { $_ -ne "" }
foreach ($n in $names) {
    if (-not $sections.Contains($n)) { throw "plan has no [$n] section" }
    if ($sections[$n].Count -eq 0) { throw "[$n] has no lines" }
}
$boots = @()
for ($r = 1; $r -le $Rounds; $r++) { foreach ($n in $names) { $boots += , @($r, $n) } }

if ($DryRun) {
    foreach ($b in $boots) {
        "# round $($b[0]) $($b[1])"
        $sections[$b[1]] | ForEach-Object { "  $_" }
    }
    return
}

# One run directory per invocation, never reused.
New-Item -ItemType Directory -Force -Path $Out | Out-Null
$k = 1
while (Test-Path (Join-Path $Out ("run-{0:D3}" -f $k))) { $k++ }
$RunDir = Join-Path $Out ("run-{0:D3}" -f $k)
New-Item -ItemType Directory -Path $RunDir | Out-Null
Copy-Item -LiteralPath $Plan -Destination (Join-Path $RunDir "plan.txt")
$Index = Join-Path $RunDir "INDEX.tsv"
"boot`tround`tname`tstart`tend`tverdict`tlog" | Set-Content -Encoding ascii $Index
$RawLog = Join-Path $RunDir "raw.log"
"order=$Order rounds=$Rounds port=$Port started=$((Get-Date).ToString('s'))" |
    Set-Content -Encoding ascii (Join-Path $RunDir "run.txt")

$sp = New-Object System.IO.Ports.SerialPort $Port, $Baud, "None", 8, "One"
$sp.ReadTimeout = 200
$sp.Encoding = [System.Text.Encoding]::GetEncoding(28591)  # bytes 1:1
$sp.Open()
# The ST-LINK hands back what it buffered before the port was opened; it is
# stale (memory: DK1 serial truths), so drop it.
Start-Sleep -Milliseconds 300
$null = $sp.ReadExisting()

$script:tail = ""          # the last few hundred characters, for matching
$script:bootLog = $null    # the current boot's log file

function Pump([int]$ms) {
    Start-Sleep -Milliseconds $ms
    $s = $sp.ReadExisting()
    if ($s.Length -gt 0) {
        [System.IO.File]::AppendAllText($RawLog, $s, $sp.Encoding)
        if ($null -ne $script:bootLog) { [System.IO.File]::AppendAllText($script:bootLog, $s, $sp.Encoding) }
        $script:tail += $s
        if ($script:tail.Length -gt 4096) { $script:tail = $script:tail.Substring($script:tail.Length - 4096) }
    }
    return $s
}

function Say([string]$text) {
    Write-Host ("[{0}] {1}" -f (Get-Date).ToString("HH:mm:ss"), $text)
}

# Wait until one of `$marks` appears in what arrives after this call, or the
# deadline. Returns the mark, or $null on timeout.
function Wait-For([string[]]$marks, [int]$seconds, [int]$nagEvery = 0, [string]$nag = "") {
    $script:tail = ""
    $deadline = (Get-Date).AddSeconds($seconds)
    $nextNag = if ($nagEvery -gt 0) { (Get-Date).AddSeconds($nagEvery) } else { $null }
    while ((Get-Date) -lt $deadline) {
        $null = Pump 50
        foreach ($m in $marks) { if ($script:tail.Contains($m)) { return $m } }
        if ($null -ne $nextNag -and (Get-Date) -gt $nextNag) {
            Say $nag
            [console]::beep(880, 300)
            $nextNag = (Get-Date).AddSeconds($nagEvery)
        }
    }
    return $null
}

function Send-Line([string]$line) {
    $sp.Write($line + "`r")
}

# Get to a U-Boot prompt: stop an autoboot that is counting, or find the
# prompt the board is already at.
function Reach-Uboot {
    Send-Line ""
    $m = Wait-For @($Prompt, $Autoboot) 5
    if ($m -eq $Prompt) { return $true }
    if ($m -ne $Autoboot) {
        $m = Wait-For @($Autoboot, $Prompt) 3600 60 "no U-Boot yet: press RESET on the DK1 (or replug USB-C if it powered off)"
    }
    if ($null -eq $m) { return $false }
    if ($m -eq $Autoboot) {
        Send-Line ""
        $m = Wait-For @($Prompt) 10
    }
    return ($m -eq $Prompt)
}

$n = 0
try {
    foreach ($b in $boots) {
        $n++
        $round = $b[0]; $name = $b[1]
        $logName = "boot-{0:D3}-{1}.log" -f $n, $name
        $script:bootLog = Join-Path $RunDir $logName
        $start = (Get-Date).ToString("s")
        Say "boot $n of $($boots.Count): round $round, $name"
        if (-not (Reach-Uboot)) {
            Say "gave up waiting for U-Boot"
            "$n`t$round`t$name`t$start`t$((Get-Date).ToString('s'))`tno-uboot`t$logName" | Add-Content -Encoding ascii $Index
            break
        }
        $lines = $sections[$name]
        $verdict = $null
        for ($i = 0; $i -lt $lines.Count; $i++) {
            $line = $lines[$i]
            Send-Line $line
            if ($i -eq $lines.Count - 1) { break }
            $m = Wait-For @($Prompt) $LineTimeout
            $bad = $UbootErrors | Where-Object { $script:tail.Contains($_) } | Select-Object -First 1
            if ($null -eq $m -or $null -ne $bad) {
                $verdict = if ($null -eq $m) { "uboot-timeout" } else { "uboot-error" }
                Say "[$name] '$line' -> $verdict $bad"
                break
            }
        }
        if ($null -eq $verdict) {
            $marks = @($EndMark) + $KernelDeath
            $m = Wait-For $marks $BootTimeout
            if ($m -eq $EndMark) {
                # The rest of the end line, then the reset the bench does.
                $null = Pump 300
                $verdict = "end"
            } elseif ($null -eq $m) {
                $verdict = "timeout"
            } else {
                $null = Pump 2000
                $verdict = "died: $m"
            }
        }
        Say "[$name] $verdict"
        "$n`t$round`t$name`t$start`t$((Get-Date).ToString('s'))`t$verdict`t$logName" | Add-Content -Encoding ascii $Index
        $script:bootLog = $null
        if ($verdict.StartsWith("uboot")) {
            # Back to a clean U-Boot for the next boot.
            Send-Line "reset"
        }
    }
} finally {
    $script:bootLog = $null
    $null = Pump 200
    $sp.Close()
    Say "logs in $RunDir"
}
