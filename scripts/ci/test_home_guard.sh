#!/usr/bin/env bash
#
# D-070, D-071, D-072: no test may read or write the real ~/.openprx, the
# per-user OpenPRX platform config directory, or another tool's credential
# store (~/.claude, ~/.qwen, ~/.codex, ~/.gemini, ~/.aws).
#
# Runs `cargo test <args>` with HOME pointed at a fresh guard home that holds
# a sentinel copy of each of those locations, and fails when any process of
# the run (a test binary or a child it spawned, whether or not its test
# tolerated the child's failure) touched one:
#   - a file created, changed or removed under a sentinel;
#   - a sentinel file read or a sentinel directory listed (access times; a
#     canary proves first that this filesystem records them).
# On macOS, additionally, `sandbox-exec` kills any process that reads or
# writes the invoking user's REAL locations, so they are never touched even
# through a path that bypasses HOME. The guard's own sentinels are not
# killed on: an access to them must happen to be seen by the checks above.
#
# D-081: the tests also run with outbound network denied except loopback,
# and any attempt to reach another address fails the run:
#   - macOS: the same sandbox profile kills (SIGKILL) any process that
#     connects or sends to a non-loopback IP address, TCP or UDP, IPv4 or
#     IPv6. Unix-domain sockets stay allowed, so the system resolver
#     (mDNSResponder) still answers: a name lookup can leave the machine, a
#     connection cannot. Detection: a test binary that tries is killed, so
#     its run fails. Another process of the run (a child the test spawned
#     and tolerated) is detected when macOS reports the denial: the kernel's
#     sandbox report, which carries a token unique to this run (reliable for
#     platform binaries such as nc, curl or git; the canary proves the
#     channel is live), or a crash report of a sandbox kill of a process of
#     this run's tree (tracked while it runs; the report names the process
#     and its parent), so a concurrent guard run cannot fail this one. macOS does not report every denial of a non-platform binary,
#     so such a child's attempt can go undetected; it is still blocked, and
#     the final message says exactly this.
#   - Linux: the tests run in a new network namespace, as the invoking user:
#     through `sudo unshare --net` then `setpriv` back to the user when sudo
#     is available (preferred; `modprobe dummy` is tried first), else an
#     unprivileged user namespace whose tests run in a nested user namespace
#     mapped back to the invoking uid and gid (util-linux 2.38 or later).
#     Its routes are loopback and a dummy interface. A packet routed off loopback goes to the dummy
#     interface, which drops it and counts it; any count fails the run. The
#     resolver must be on loopback (systemd-resolved's 127.0.0.53), or a
#     lookup counts as an attempt. `ip` (iproute2), `setpriv` and the
#     `dummy` kernel module are required.
#   A canary proves first that an outbound attempt is caught. Where the
#   network cannot be denied, the guard refuses to run.
# The test targets are built first without the network denial (a dependency
# may have to be fetched); the tests then run offline (CARGO_NET_OFFLINE).
#
# CI runs the test suite through this script (.github/workflows/ci-run.yml).
# Usage: scripts/ci/test_home_guard.sh [cargo test args...]

set -euo pipefail

real_home="${HOME:?HOME must be set}"
export CARGO_HOME="${CARGO_HOME:-$real_home/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$real_home/.rustup}"

# The physical path: sandbox rules match resolved paths (/var is a symlink
# on macOS).
guard_home="$(cd "$(mktemp -d)" && pwd -P)"
profile="$guard_home.sb"
build_profile="$guard_home.build.sb"
linux_inner="$guard_home.net.sh"
canary_profile="$guard_home.canary.sb"
denials_log="$guard_home.denials.log"
log_pid=""
# Every file of the run is "$guard_home" or "$guard_home.<name>".
trap 'if [ -n "$log_pid" ]; then kill "$log_pid" 2>/dev/null; fi; rm -rf "$guard_home" "$guard_home".*' EXIT
# TEST-NET-1 (RFC 5737): never routed on the internet, so even a canary
# the guard failed to stop goes nowhere.
canary_address="192.0.2.1"

# Locations under a home that tests must never touch, relative to the home.
# The platform config directory is where `directories::ProjectDirs` puts
# OpenPRX's per-user config (the Copilot token cache): macOS and Linux forms.
protected=(
    ".openprx"
    ".claude"
    ".qwen"
    ".codex"
    ".gemini"
    ".aws"
    "Library/Application Support/openprx"
    ".config/openprx"
)

sentinel_paths=()
for relative in "${protected[@]}"; do
    dir="$guard_home/$relative"
    mkdir -p "$dir"
    printf 'sentinel\n' >"$dir/sentinel"
    sentinel_paths+=("$dir" "$dir/sentinel")
done
# Files the code under test would look for first.
printf 'default_provider = "sentinel"\n' >"$guard_home/.openprx/config.toml"
printf '{}\n' >"$guard_home/.claude/.credentials.json"
printf '{}\n' >"$guard_home/.qwen/oauth_creds.json"
printf '{}\n' >"$guard_home/.codex/auth.json"
printf '[default]\n' >"$guard_home/.aws/credentials"
sentinel_paths+=(
    "$guard_home/.openprx/config.toml"
    "$guard_home/.claude/.credentials.json"
    "$guard_home/.qwen/oauth_creds.json"
    "$guard_home/.codex/auth.json"
    "$guard_home/.aws/credentials"
)

# Access time of each path, one per line, by stat alone (stat does not
# change it). GNU stat first, then BSD stat.
access_times() {
    stat -c '%X %n' "$@" 2>/dev/null || stat -f '%a %N' "$@"
}

# Names and contents of everything under the sentinels.
snapshot() {
    local relative
    for relative in "${protected[@]}"; do
        (cd "$guard_home" && find "$relative" | LC_ALL=C sort && find "$relative" -type f -exec cksum {} + | LC_ALL=C sort)
    done
}

before="$(snapshot)"

# An access time older than the modification time: the next read of a file,
# or listing of a directory, moves it forward (relatime and strict atime).
touch -a -t 200001010000 "${sentinel_paths[@]}"

# Canary: prove this filesystem records reads before relying on it.
canary="$guard_home/.guard-canary"
printf 'canary\n' >"$canary"
touch -a -t 200001010000 "$canary"
canary_before="$(access_times "$canary")"
cat "$canary" >/dev/null
if [ "$(access_times "$canary")" = "$canary_before" ]; then
    echo "test_home_guard: this filesystem does not record access times (noatime);" \
        "reads under the guard home cannot be detected. Run on a relatime or strictatime mount." >&2
    exit 2
fi

atimes_before="$(access_times "${sentinel_paths[@]}")"

# The sandbox profile: the real protected locations, and (for the tests) the
# network. The last matching rule wins.
write_profile() {
    local deny_network="$1" tag="$2"
    printf '(version 1)\n(allow default)\n'
    if [ "$deny_network" = 1 ]; then
        printf '(deny network-outbound (remote ip) (with send-signal SIGKILL) (with message "%s-network"))\n' "$tag"
        printf '(allow network-outbound (remote ip "localhost:*"))\n'
    fi
    # The real locations only, by their physical path (sandbox rules match
    # resolved paths), whether or not they exist yet.
    local real_home_physical relative real
    real_home_physical="$(cd "$real_home" && pwd -P)"
    for relative in "${protected[@]}"; do
        real="$real_home_physical/$relative"
        if [ -e "$real" ]; then
            real="$(cd "$real" && pwd -P)"
        fi
        printf '(deny file-read-data file-write* (subpath "%s") (with send-signal SIGKILL) (with message "%s-file"))\n' "$real" "$tag"
    done
}

# Run inside the Linux test namespace, as its root: bring loopback up, route
# everything else to a counting dummy interface, prove the count works, run
# the tests (as the invoking user when started through sudo) and record how
# many packets tried to leave.
cat >"$linux_inner" <<'INNER'
set -euo pipefail
ip link set lo up
ip link add prx-guard0 type dummy
# Avoid kernel-generated link-local DAD/router solicitations being counted as
# test traffic; the explicit IPv6 source below already uses nodad.
ip link set dev prx-guard0 addrgenmode none
ip link set prx-guard0 up
ip addr add 192.0.2.2/32 dev prx-guard0
ip route add default dev prx-guard0
if ! { ip -6 addr add 2001:db8::2/128 dev prx-guard0 nodad && ip -6 route add default dev prx-guard0; } 2>/dev/null; then
    echo "test_home_guard: no IPv6 route in the test namespace: an IPv6 attempt fails unrouted (it cannot leave) but is not counted" >&2
fi
sent() {
    # /sys may still be mounted for the invoking network namespace after
    # unshare --net. Query this namespace's interface over netlink instead.
    LC_ALL=C ip -s link show dev prx-guard0 | awk '
        /^[[:space:]]*TX:/ {
            getline
            if ($2 ~ /^[0-9]+$/) { print $2; found = 1 }
            exit
        }
        END { if (!found) exit 1 }
    '
}
before="$(sent)"
timeout 2 bash -c "exec 3<>/dev/tcp/$PRX_GUARD_CANARY/9" 2>/dev/null || true
if [ "$(sent)" -le "$before" ]; then
    echo "test_home_guard: canary: an outbound connection in the test namespace was not counted" >&2
    exit 2
fi
baseline="$(sent)"
status=0
# The tests never run as root, not even namespace root: its capabilities
# would let them write files the invoking user cannot.
ids="${PRX_GUARD_RUN_AS#*:}"
case "${PRX_GUARD_RUN_AS%%:*}" in
setpriv) setpriv --reuid="${ids%%:*}" --regid="${ids##*:}" --init-groups -- "$@" || status=$? ;;
userns) unshare --user --map-user="${ids%%:*}" --map-group="${ids##*:}" -- "$@" || status=$? ;;
*)
    echo "test_home_guard: unknown PRX_GUARD_RUN_AS '$PRX_GUARD_RUN_AS'" >&2
    exit 2
    ;;
esac
echo "$(($(sent) - baseline))" >"$PRX_GUARD_ATTEMPTS_FILE"
exit "$status"
INNER

# Record every process of the tree under `$1` into `$2` while `$1` runs.
track_tree() {
    local root="$1" out="$2" known pid ppid
    known=" $root "
    printf '%s\n' "$root" >>"$out"
    while kill -0 "$root" 2>/dev/null; do
        while read -r pid ppid; do
            case "$known" in
            *" $ppid "*)
                case "$known" in
                *" $pid "*) ;;
                *)
                    known="$known$pid "
                    printf '%s\n' "$pid" >>"$out"
                    ;;
                esac
                ;;
            esac
        done < <(ps -axo pid=,ppid=)
        sleep 0.1
    done
}

status=0
network_attempts=0
file_denials=0
sandbox_killed=0
all_clear="no process of the test run touched a protected home location or tried to reach a non-loopback address"
attempts_file="$guard_home/.guard-network-attempts"
case "$(uname -s)" in
Darwin)
    if ! command -v sandbox-exec >/dev/null 2>&1; then
        echo "test_home_guard: sandbox-exec is missing; the network cannot be denied, so the tests do not run." >&2
        exit 2
    fi
    all_clear="no protected home location was touched and no attempt to reach a non-loopback address was detected (a test binary that tries is killed; another process is detected when macOS reports the denial, which it does not always do for a non-platform binary; every attempt is blocked either way)"
    guard_token="prx-test-guard-$$-$(date +%s)-$RANDOM"
    write_profile 0 "$guard_token-build" >"$build_profile"
    write_profile 1 "$guard_token-canary" >"$canary_profile"
    write_profile 1 "$guard_token-run" >"$profile"
    HOME="$guard_home" sandbox-exec -f "$build_profile" cargo test --no-run "$@" || status=$?
    if [ "$status" -eq 0 ]; then
        # The kernel reports every denial with the rule's message: this run's
        # token. A denial by a child process the test tolerated is reported
        # like one by the test binary.
        /usr/bin/log stream --style compact \
            --predicate "sender == \"Sandbox\" AND eventMessage CONTAINS \"$guard_token\"" >"$denials_log" 2>&1 &
        log_pid=$!
        sleep 2
        canary_status=0
        sandbox-exec -f "$canary_profile" /usr/bin/nc -z -G 2 "$canary_address" 9 >/dev/null 2>&1 &
        canary_pid=$!
        wait "$canary_pid" 2>/dev/null || canary_status=$?
        if [ "$canary_status" -ne 137 ]; then
            echo "test_home_guard: canary: an outbound connection was not killed (exit $canary_status); the network is not denied, so the tests do not run." >&2
            exit 2
        fi
        for _ in $(seq 1 40); do
            grep -q -- "$guard_token-canary-network" "$denials_log" && break
            sleep 0.5
        done
        if ! grep -q -- "$guard_token-canary-network" "$denials_log"; then
            echo "test_home_guard: canary: the sandbox's report of a denied connection did not arrive in the log; a denial by a child process could not be seen, so the tests do not run." >&2
            exit 2
        fi
        run_marker="$guard_home.run-start"
        touch "$run_marker"
        run_start="$(date +%Y-%m-%d-%H%M%S)"
        run_tree="$guard_home.run-tree"
        # Every test binary cargo starts goes through this runner, which
        # records its pid and execs the binary (same pid): a crash report of
        # a test binary, or of its child, then names a pid of this run even
        # when the binary lived too briefly for the tree tracker to see it.
        host_triple="$(rustc -vV | sed -n 's/^host: //p')"
        runner_var="CARGO_TARGET_$(printf '%s' "$host_triple" | tr '[:lower:]-' '[:upper:]_')_RUNNER"
        runner_script="$guard_home.runner.sh"
        printf '#!/bin/sh\necho "$$" >>"$PRX_GUARD_RUN_PIDS"\nexec ${PRX_GUARD_INNER_RUNNER:-} "$@"\n' >"$runner_script"
        chmod +x "$runner_script"
        HOME="$guard_home" CARGO_NET_OFFLINE=true PRX_GUARD_RUN_PIDS="$run_tree" \
            PRX_GUARD_INNER_RUNNER="${!runner_var:-}" env "$runner_var=$runner_script" \
            sandbox-exec -f "$profile" cargo test "$@" &
        run_pid=$!
        track_tree "$run_pid" "$run_tree" &
        tracker_pid=$!
        wait "$run_pid" || status=$?
        wait "$tracker_pid" 2>/dev/null || true
        # Reports are delivered asynchronously.
        sleep 3
        # Crash reports of processes the sandbox killed during the run (the
        # canary excluded).
        sandbox_kills=()
        reports_dir="$real_home/Library/Logs/DiagnosticReports"
        if [ -d "$reports_dir" ]; then
            # A report's file name carries the time of the crash; its
            # modification time can move later (the reporter rewrites it).
            while IFS= read -r report; do
                crashed="$(basename "$report" | sed -nE 's/.*-([0-9]{4}-[0-9]{2}-[0-9]{2}-[0-9]{6})(\.[0-9]+)?\.ips$/\1/p')"
                if [ -z "$crashed" ] || [[ "$crashed" < "$run_start" ]]; then
                    continue
                fi
                if ! grep -q '"namespace" *: *"SANDBOX"' "$report"; then
                    continue
                fi
                # Only a process of this run's tree, or a child of one: a
                # concurrent guard run's kills are its own (the canary is
                # not in the tree either).
                killed_pid="$(grep -oE '"pid" *: *[0-9]+' "$report" | head -1 | grep -oE '[0-9]+$' || true)"
                killed_parent="$(grep -oE '"parentPid" *: *[0-9]+' "$report" | head -1 | grep -oE '[0-9]+$' || true)"
                if { [ -n "$killed_pid" ] && grep -qxF "$killed_pid" "$run_tree"; } ||
                    { [ -n "$killed_parent" ] && grep -qxF "$killed_parent" "$run_tree"; }; then
                    sandbox_kills+=("$report")
                fi
            done < <(find "$reports_dir" -maxdepth 1 -name '*.ips' -newer "$run_marker" 2>/dev/null)
        fi
        kill "$log_pid" 2>/dev/null || true
        wait "$log_pid" 2>/dev/null || true
        log_pid=""
        network_attempts="$(grep -c -- "$guard_token-run-network" "$denials_log" || true)"
        file_denials="$(grep -c -- "$guard_token-run-file" "$denials_log" || true)"
        if [ "$network_attempts" -ne 0 ] || [ "$file_denials" -ne 0 ]; then
            echo "test_home_guard: the sandbox denied (and killed) these processes of the test run:" >&2
            grep -B1 -- "$guard_token-run" "$denials_log" | grep 'Sandbox:' | sed 's/.*Sandbox: /  /' >&2 || true
        fi
        if [ "${#sandbox_kills[@]}" -ne 0 ]; then
            echo "test_home_guard: the sandbox killed a process of the test run (crash reports):" >&2
            printf '  %s\n' "${sandbox_kills[@]}" >&2
            sandbox_killed="${#sandbox_kills[@]}"
        fi
    fi
    ;;
Linux)
    HOME="$guard_home" cargo test --no-run "$@" || status=$?
    if [ "$status" -eq 0 ]; then
        if sudo -n true 2>/dev/null && sudo -n unshare --net true 2>/dev/null; then
            sudo -n modprobe dummy 2>/dev/null || true
            runner=(sudo -n -E env)
            namespace=(unshare --net --)
            run_as="setpriv:$(id -u):$(id -g)"
        elif unshare --user --map-root-user --net true 2>/dev/null && unshare --help 2>&1 | grep -q -- '--map-user'; then
            runner=(env)
            namespace=(unshare --user --map-root-user --net --)
            run_as="userns:$(id -u):$(id -g)"
        else
            echo "test_home_guard: cannot create a network namespace for the tests as the invoking user (no sudo, and no unprivileged user namespaces with 'unshare --map-user'); the network cannot be denied, so the tests do not run." >&2
            exit 2
        fi
        # Passed explicitly: sudo's policy may reset HOME and the rest even
        # with -E.
        guard_env=(
            "PATH=$PATH" "HOME=$guard_home" "CARGO_HOME=$CARGO_HOME" "RUSTUP_HOME=$RUSTUP_HOME"
            "CARGO_NET_OFFLINE=true" "PRX_GUARD_RUN_AS=$run_as" "PRX_GUARD_ATTEMPTS_FILE=$attempts_file"
            "PRX_GUARD_CANARY=$canary_address"
        )
        if [ -n "${CARGO_TARGET_DIR:-}" ]; then
            guard_env+=("CARGO_TARGET_DIR=$CARGO_TARGET_DIR")
        fi
        # sudo removes TMPDIR even with -E. Keep the build wrapper's private,
        # fresh test workspace instead of silently falling back to shared /tmp.
        if [ -n "${TMPDIR:-}" ]; then
            guard_env+=("TMPDIR=$TMPDIR")
        fi
        # Keep the invoking user's ownership under a private umask. The
        # namespace-root writer truncates this file, then the outer guard must
        # read it without broadening its permissions.
        : >"$attempts_file"
        "${runner[@]}" "${guard_env[@]}" "${namespace[@]}" bash "$linux_inner" "$(command -v cargo)" test "$@" ||
            status=$?
        if [ ! -s "$attempts_file" ]; then
            echo "test_home_guard: the network guard did not complete (is the dummy module available? try 'sudo modprobe dummy')." >&2
            exit "$((status == 0 ? 2 : status))"
        fi
        network_attempts="$(cat "$attempts_file")"
    fi
    ;;
*)
    echo "test_home_guard: no way to deny the network on $(uname -s); the tests do not run." >&2
    exit 2
    ;;
esac

# Read before anything below lists the sentinels.
atimes_after="$(access_times "${sentinel_paths[@]}")"
after="$(snapshot)"

failed=0
if [ "$before" != "$after" ]; then
    echo "test_home_guard: tests wrote under a protected home location:" >&2
    diff <(printf '%s\n' "$before") <(printf '%s\n' "$after") >&2 || true
    failed=1
fi
if [ "$atimes_before" != "$atimes_after" ]; then
    echo "test_home_guard: tests read a protected home location:" >&2
    diff <(printf '%s\n' "$atimes_before") <(printf '%s\n' "$atimes_after") >&2 || true
    failed=1
fi
if [ "$network_attempts" -ne 0 ]; then
    echo "test_home_guard: tests tried to reach a non-loopback address ($network_attempts denied attempts or dropped packets); nothing left the machine." >&2
    failed=1
fi
if [ "$sandbox_killed" -ne 0 ]; then
    failed=1
fi
if [ "$file_denials" -ne 0 ]; then
    echo "test_home_guard: tests read or wrote the invoking user's real protected location ($file_denials denials)." >&2
    failed=1
fi
if [ "$status" -ne 0 ]; then
    echo "test_home_guard: cargo test failed (exit $status)." >&2
    exit "$status"
fi
if [ "$failed" -ne 0 ]; then
    exit 1
fi

echo "test_home_guard: $all_clear"
