#!/usr/bin/env bash
# Guarantee RAM + swap >= TARGET_MIB (default 24576 MiB) on a GitHub-hosted runner.
# The prx crate's cold clippy and test units can exceed the runner's 16 GiB RAM, and the
# image only has 4 GiB /mnt swap when the VM got an Azure resource disk. Without enough
# swap the runner is starved and lost mid-build (exit 143, or no log at all).
# Idempotent: an active swap file is already counted in SwapTotal, so a rerun adds nothing.
# Usage: scripts/ci/ensure_hosted_swap.sh [TARGET_MIB]
set -euo pipefail

target_mib="${1:-24576}"
if ! [[ "$target_mib" =~ ^[0-9]+$ ]]; then
    echo "::error::target must be a whole number of MiB, got '${target_mib}'"
    exit 1
fi

free -m
swapon --show

ram_mib=$(awk '/MemTotal/{print int($2/1024)}' /proc/meminfo)
swap_mib=$(awk '/SwapTotal/{print int($2/1024)}' /proc/meminfo)
need_mib=$(( target_mib - ram_mib - swap_mib ))
if [ "$need_mib" -le 0 ]; then
    echo "RAM ${ram_mib} MiB + swap ${swap_mib} MiB already >= ${target_mib} MiB"
    exit 0
fi

dir=/
if mountpoint -q /mnt && [ "$(df -Pm /mnt | awk 'NR==2{print $4}')" -gt "$(df -Pm / | awk 'NR==2{print $4}')" ]; then
    dir=/mnt
fi
swapfile="${dir%/}/prx-ci-swapfile"
active_swaps="$(swapon --show=NAME --noheadings)"
if grep -qxF -- "$swapfile" <<<"$active_swaps"; then
    echo "::error::${swapfile} is already active, yet RAM ${ram_mib} MiB + swap ${swap_mib} MiB < ${target_mib} MiB"
    exit 1
fi
# An inactive leftover from an earlier attempt would leave a file of the wrong size.
if [ -e "$swapfile" ]; then
    sudo rm -f -- "$swapfile"
fi

# Keep 6 GiB free for the target dir growth and build artefacts.
avail_mib=$(df -Pm "$dir" | awk 'NR==2{print $4}')
if [ $(( avail_mib - need_mib )) -lt 6144 ]; then
    echo "::error::cannot add ${need_mib} MiB swap on ${dir}: only ${avail_mib} MiB free"
    exit 1
fi

sudo fallocate -l "${need_mib}M" "$swapfile"
sudo chmod 600 "$swapfile"
sudo mkswap "$swapfile" >/dev/null
sudo swapon "$swapfile"

free -m
swapon --show
