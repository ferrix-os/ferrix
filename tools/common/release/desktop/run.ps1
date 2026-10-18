<#
Boot Ferrix's x86-64 desktop in QEMU on Windows, from the release archive
this script came in. Nothing but QEMU is needed: no Rust, no build.

    powershell -ExecutionPolicy Bypass -File run.ps1
    powershell -ExecutionPolicy Bypass -File run.ps1 -Tcg -Memory 4096 -Size 1920x1080
    powershell -ExecutionPolicy Bypass -File run.ps1 -Vnc 5     (no window: VNC on 127.0.0.1:5905)

QEMU's window shows the desktop; this console shows the serial log, and
Ctrl-A x in it quits QEMU, as closing the window does.

-Qemu names another qemu-system-x86_64.exe; -OvmfCode and -OvmfVars name the
UEFI firmware, when it is not the one QEMU's installer puts in its share
folder.

Under the Windows Hypervisor Platform (Windows features: "Windows Hypervisor
Platform") the guest runs on the processor; without it QEMU emulates, which
is slow. Released QEMU's WHPX support faults Ferrix's ring-3 drivers with
more than one processor, so the guest gets one there unless -Smp says
otherwise. Kept in step with run.sh.
#>
param(
    [string]$Qemu = "",
    [int]$Memory = 2048,
    [int]$Smp = 0,
    [string]$Size = "1280x800",
    [int]$Vnc = -1,
    [switch]$Tcg,
    [string]$OvmfCode = "",
    [string]$OvmfVars = "",
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$Extra = @()
)

$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$image = Join-Path $here "ferrix-x86_64.img"
if (-not (Test-Path $image)) {
    throw "no $image : run this script from the unpacked archive"
}

# QEMU: the one named, the one on PATH, or where its installer puts it.
if ($Qemu -eq "") {
    $found = Get-Command qemu-system-x86_64.exe -ErrorAction SilentlyContinue
    if ($null -ne $found) {
        $Qemu = $found.Source
    } elseif (Test-Path "$env:ProgramFiles\qemu\qemu-system-x86_64.exe") {
        $Qemu = "$env:ProgramFiles\qemu\qemu-system-x86_64.exe"
    } else {
        throw "qemu-system-x86_64.exe not found. Install QEMU (winget install SoftwareFreedomConservancy.QEMU) or pass -Qemu <path>."
    }
}
$version = (& $Qemu --version | Select-Object -First 1)

# The firmware QEMU's installer ships: EDK2's OVMF, and its variable store.
if ($OvmfCode -eq "") {
    $share = Join-Path (Split-Path -Parent $Qemu) "share"
    $OvmfCode = Join-Path $share "edk2-x86_64-code.fd"
    if ($OvmfVars -eq "") {
        $OvmfVars = Join-Path $share "edk2-i386-vars.fd"
    }
}
if (-not (Test-Path $OvmfCode)) {
    throw "no UEFI firmware at $OvmfCode; pass -OvmfCode (and -OvmfVars)"
}

# The accelerator: WHPX when QEMU has it and Windows lets it start, else
# TCG. Starting it is the only way to know: QEMU initialises an
# accelerator only when it starts a machine, so a halted one is started
# and watched for a moment, as xtask does (`usable` in qemu.rs).
$accel = "tcg"
if (-not $Tcg) {
    $offered = (& $Qemu -accel help) -join "`n"
    if ($offered -match "(?m)^whpx\s*$") {
        $probe = Start-Process -FilePath $Qemu -PassThru -WindowStyle Hidden -ArgumentList @(
            "-accel", "whpx", "-machine", "q35", "-nodefaults", "-display", "none", "-S")
        Start-Sleep -Seconds 3
        if (-not $probe.HasExited) {
            $accel = "whpx"
            Stop-Process -Id $probe.Id -Force
        } else {
            Write-Host "run.ps1: WHPX did not start (is the Windows Hypervisor Platform feature on?): emulating"
        }
    }
}

# Processors: one under a WHPX QEMU without fetch-qemu-windows.sh's patch,
# whose version says `whpx-gva`; four otherwise.
if ($Smp -eq 0) {
    if ($accel -eq "whpx" -and $version -notmatch "whpx-gva") {
        $Smp = 1
        Write-Host "run.ps1: one processor under WHPX: released QEMU's WHPX faults Ferrix's drivers with more (-Smp N overrides)"
    } else {
        $Smp = 4
    }
}

# The processor xtask's boots have (`x86_cpu` in qemu.rs): x86-64-v3 with
# SMEP, SMAP and UMIP; under WHPX the speculation controls and an
# invariant TSC too.
$cpu = "qemu64,+pdpe1gb,+smep,+smap,+umip,+rdrand,+rdseed,+ssse3,+sse4.1,+sse4.2,+popcnt," +
    "+cx16,+movbe,+xsave,+xsaveopt,+avx,+avx2,+f16c,+fma,+bmi1,+bmi2,+abm,+pclmulqdq,+aes,+x2apic"
if ($accel -eq "whpx") {
    $cpu += ",+spec-ctrl,+stibp,+ssbd,+arch-capabilities,+auto-ibrs,+invtsc"
}

# A fresh variable store every boot, as xtask makes one.
$vars = Join-Path $env:TEMP ("ferrix-vars-" + [guid]::NewGuid().ToString("N") + ".fd")
if (Test-Path $OvmfVars) {
    Copy-Item $OvmfVars $vars
} else {
    New-Item -ItemType File -Path $vars | Out-Null
}

$width, $height = $Size.Split("x")
$flags = "disable-legacy=on,iommu_platform=on"
$arguments = @(
    "-accel", $accel, "-machine", "q35", "-cpu", $cpu,
    "-m", "$Memory", "-smp", "$Smp",
    "-device", "isa-debug-exit,iobase=0xf4,iosize=0x04",
    "-device", "intel-iommu,intremap=on,eim=off",
    "-vga", "none",
    "-drive", "format=raw,file=$image,if=none,id=disk",
    "-device", "ide-hd,drive=disk,bus=ide.0,bootindex=0",
    "-device", "pcie-root-port,id=ferrix.port0,chassis=1,slot=1",
    "-device", "virtio-rng-pci,$flags,bus=ferrix.port0",
    "-device", "virtio-gpu-pci,id=gpu0,addr=0x10,$flags,ioeventfd=on,edid=on,xres=$width,yres=$height",
    "-device", "virtio-keyboard-pci,$flags",
    "-device", "virtio-tablet-pci,$flags",
    "-device", "VGA,addr=0xa",
    "-netdev", "user,id=net0",
    "-device", "virtio-net-pci,netdev=net0,mac=52:54:00:12:34:56,$flags",
    "-drive", "if=pflash,format=raw,readonly=on,file=$OvmfCode",
    "-drive", "if=pflash,format=raw,file=$vars",
    "-serial", "mon:stdio",
    "-no-reboot"
)
if ($Vnc -ge 0) {
    $arguments += @("-display", "vnc=127.0.0.1:$Vnc,display=gpu0")
    Write-Host "run.ps1: the screen is on VNC at 127.0.0.1:$(5900 + $Vnc)"
}
$arguments += $Extra

Write-Host "run.ps1: $version, under $accel with $Smp processors, firmware $OvmfCode"
try {
    & $Qemu @arguments
} finally {
    Remove-Item $vars -ErrorAction SilentlyContinue
}
