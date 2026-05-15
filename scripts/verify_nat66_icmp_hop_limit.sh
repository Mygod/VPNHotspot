#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage:
  sudo scripts/verify_nat66_icmp_hop_limit.sh [iface]

Environment overrides:
  IFACE=...    Downstream USB-tether interface. Defaults to the first IPv6
               default-route interface with a ULA source address.
  SRC=...      Source IPv6 address on IFACE. Defaults to the first ULA address.
  TARGET=...   External IPv6 Echo target. Defaults to 2606:4700:4700::1111.
  GW=...       IPv6 gateway on IFACE. Defaults to IFACE's IPv6 default gateway.
  SRC_MAC=...  Source Ethernet address. Defaults to IFACE's MAC address.
  DST_MAC=...  Destination Ethernet address. Defaults to GW's neighbour MAC.

The script sends one raw Ethernet ICMPv6 Echo Request with hop limit 1 and
captures ICMPv6 on IFACE. It exits 0 only if the capture contains an ICMPv6
Time Exceeded response.
EOF
}

if [[ ${1:-} == "-h" || ${1:-} == "--help" ]]; then
    usage
    exit 0
fi

if (( EUID != 0 )); then
    echo "Run this with sudo; nping raw Ethernet and tcpdump need root." >&2
    exit 2
fi

for command in ip nping tcpdump awk grep cat mktemp ping sleep tee; do
    if ! command -v "$command" >/dev/null; then
        echo "Missing required command: $command" >&2
        exit 2
    fi
done

TARGET=${TARGET:-2606:4700:4700::1111}
IFACE=${IFACE:-${1:-}}

first_ipv6_addr() {
    local iface=$1
    local addr
    while read -r addr; do
        addr=${addr%%/*}
        case ${addr,,} in
            fc*|fd*)
                echo "$addr"
                return 0
                ;;
        esac
    done < <(ip -o -6 addr show dev "$iface" scope global | awk '{ print $4 }')
    ip -o -6 addr show dev "$iface" scope global | awk 'NR == 1 { split($4, a, "/"); print a[1] }'
}

if [[ -z $IFACE ]]; then
    while read -r candidate; do
        if ip -o -6 addr show dev "$candidate" scope global |
            awk '{ split($4, a, "/"); print tolower(a[1]) }' |
            grep -Eq '^(fc|fd)'; then
            IFACE=$candidate
            break
        fi
    done < <(ip -6 route show default |
        awk '{ for (i = 1; i <= NF; ++i) if ($i == "dev") print $(i + 1) }' |
        awk '!seen[$0]++')
fi

if [[ -z $IFACE ]]; then
    echo "Could not infer USB-tether interface. Pass it as the first argument or set IFACE=..." >&2
    exit 2
fi

SRC=${SRC:-$(first_ipv6_addr "$IFACE")}
if [[ -z $SRC ]]; then
    echo "Could not infer source IPv6 address on $IFACE. Set SRC=..." >&2
    exit 2
fi

GW=${GW:-$(ip -6 route show default dev "$IFACE" |
    awk '{ for (i = 1; i <= NF; ++i) if ($i == "via") { print $(i + 1); exit } }')}
if [[ -z $GW ]]; then
    echo "Could not infer IPv6 gateway on $IFACE. Set GW=..." >&2
    exit 2
fi

SRC_MAC=${SRC_MAC:-$(cat "/sys/class/net/$IFACE/address")}
if [[ -z $SRC_MAC ]]; then
    echo "Could not infer source MAC for $IFACE. Set SRC_MAC=..." >&2
    exit 2
fi

if [[ -z ${DST_MAC:-} ]]; then
    # Populate neighbour state if it has expired. The probe is best effort; the
    # following neighbour lookup is the source of truth.
    ping -6 -c 1 -W 1 "${GW}%${IFACE}" >/dev/null 2>&1 || true
    DST_MAC=$(ip -6 neigh show "$GW" dev "$IFACE" |
        awk '{ for (i = 1; i <= NF; ++i) if ($i == "lladdr") { print $(i + 1); exit } }')
fi
if [[ -z $DST_MAC ]]; then
    echo "Could not infer gateway MAC for $GW on $IFACE. Set DST_MAC=..." >&2
    exit 2
fi

tmpdir=$(mktemp -d)
tcpdump_log=$tmpdir/tcpdump.log
nping_log=$tmpdir/nping.log
tcpdump_pid=

cleanup() {
    if [[ -n ${tcpdump_pid:-} ]] && kill -0 "$tcpdump_pid" >/dev/null 2>&1; then
        kill "$tcpdump_pid" >/dev/null 2>&1 || true
        wait "$tcpdump_pid" >/dev/null 2>&1 || true
    fi
    rm -rf "$tmpdir"
}
trap cleanup EXIT INT TERM

echo "iface=$IFACE"
echo "source_ipv6=$SRC"
echo "source_mac=$SRC_MAC"
echo "gateway_ipv6=$GW"
echo "gateway_mac=$DST_MAC"
echo "target=$TARGET"

tcpdump -i "$IFACE" -n -vv -l icmp6 >"$tcpdump_log" 2>&1 &
tcpdump_pid=$!
sleep 0.3

set +e
nping -6 --icmp --send-eth -N -e "$IFACE" \
    --source-mac "$SRC_MAC" --dest-mac "$DST_MAC" \
    -S "$SRC" --hop-limit 1 -c 1 --privileged "$TARGET" | tee "$nping_log"
nping_status=${PIPESTATUS[0]}
set -e

sleep 1
if [[ -n ${tcpdump_pid:-} ]] && kill -0 "$tcpdump_pid" >/dev/null 2>&1; then
    kill "$tcpdump_pid" >/dev/null 2>&1 || true
    wait "$tcpdump_pid" >/dev/null 2>&1 || true
    tcpdump_pid=
fi

echo
echo "--- tcpdump icmp6 capture ---"
cat "$tcpdump_log"

if grep -qi 'echo reply' "$tcpdump_log"; then
    echo
    echo "FAIL: observed an Echo Reply for the hop-limit-1 probe; the original Echo was not fully consumed."
    echo "nping exit status: $nping_status"
    exit 1
fi

if grep -qi 'time exceeded' "$tcpdump_log"; then
    echo
    echo "PASS: observed ICMPv6 Time Exceeded and no Echo Reply for the hop-limit-1 Echo probe."
    exit 0
fi

echo
echo "FAIL: no ICMPv6 Time Exceeded was observed."
echo "nping exit status: $nping_status"
exit 1
