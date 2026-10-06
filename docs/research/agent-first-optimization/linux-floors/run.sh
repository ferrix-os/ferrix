#!/bin/bash
# Boot the Linux guest under bench-ipc's QEMU+KVM line and -cpu model, one
# vCPU, QEMU pinned to host core 11, 512 MiB (bench-ipc's default --memory),
# serial to a log. No disk, no network.
#   ./run.sh <tag> <default|off> [host-core]
here=$(cd "$(dirname "$0")" && pwd)
tag=$1; mit=$2; core=${3:-11}
logs=$HOME/.local/share/ferrix/logs/po7-linux
# tools/common/xtask/src/qemu.rs x86_cpu("kvm") at main 3b9b1e267, verbatim.
cpu="qemu64,+pdpe1gb,+smep,+smap,+umip,+rdrand,+rdseed,+ssse3,+sse4.1,+sse4.2,+popcnt,+cx16,+movbe,+xsave,+xsaveopt,+avx,+avx2,+f16c,+fma,+bmi1,+bmi2,+abm,+pclmulqdq,+aes,+x2apic,+spec-ctrl,+stibp,+ssbd,+arch-capabilities,+auto-ibrs,+invtsc,+eraps"
append="console=ttyS0 panic=-1"
[ "$mit" = off ] && append="$append mitigations=off"
log="$logs/$tag.log"
load=$(cut -d' ' -f1-3 /proc/loadavg)
cmd=(taskset -c "$core" qemu-system-x86_64 -machine q35,kernel-irqchip=split -accel kvm -cpu "$cpu" -smp 1 -m 512
     -display none -monitor none -nic none -serial "file:$log" -no-reboot
     -kernel "$here/vmlinuz-7.0.0-29-generic" -initrd "$here/initramfs.cpio" -append "$append")
busy() { awk -v c="cpu$1" '$1==c {print $2+$3+$4+$7+$8, $2+$3+$4+$5+$6+$7+$8}' /proc/stat; }
sib=$(awk -F'[,-]' -v c=$core '{for(i=1;i<=NF;i++) if ($i!=c) print $i}' /sys/devices/system/cpu/cpu$core/topology/thread_siblings_list)
{ echo "# $(date -Is) linux mit=$mit core=$core loadavg=$load"; echo "# ${cmd[*]}"; } > "$log.meta"
read b0 t0 <<< "$(busy $sib)"
start=$(date +%s.%N)
"${cmd[@]}" &
pid=$!
for i in $(seq 1 600); do
  sleep 0.5
  grep -q "LB done\|Kernel panic\|LB error" "$log" 2>/dev/null && { sleep 1; break; }
  kill -0 $pid 2>/dev/null || break
done
kill $pid 2>/dev/null; wait $pid 2>/dev/null
read b1 t1 <<< "$(busy $sib)"
echo "# end loadavg=$(cut -d' ' -f1-3 /proc/loadavg) secs=$(echo "$(date +%s.%N) - $start" | bc) smt-sibling-cpu$sib-busy=$(( 100 * (b1 - b0) / (t1 - t0 + 1) ))%" >> "$log.meta"
cat "$log.meta"; grep '^LB ' "$log" | grep -v 'rep=[1234] '
