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
  NAT_GW=...   NAT66 gateway ULA. Defaults to the first address in SRC's prefix.
  ROUTER=...   IPv6 next hop used to infer DST_MAC. Defaults to IFACE's
               IPv6 default gateway. GW is accepted as a compatibility alias.
  SRC_MAC=...  Source Ethernet address. Defaults to IFACE's MAC address.
  DST_MAC=...  Destination Ethernet address. Defaults to ROUTER's neighbour MAC.
  TCP_URL=...  IPv6 HTTPS URL for TCP smoke test. Defaults to https://ifconfig.co/ip.
  DNS_NAME=... DNS name for UDP smoke test. Defaults to example.com.

The script runs host-side NAT66 ICMPv6 smoke checks over IFACE:

- gateway Echo
- public Echo
- captured public Echo Reply source preservation
- local Echo hop-limit-1 Time Exceeded with no leaked Echo Reply
- best-effort upstream Echo hop-limit-2 Time Exceeded
- UDP DNS
- UDP hop-limit-1 and hop-limit-2 Time Exceeded through the IPv6 error queue
- TCP HTTPS
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
for command in curl python3; do
    if ! command -v "$command" >/dev/null; then
        echo "Missing required command: $command" >&2
        exit 2
    fi
done

TARGET=${TARGET:-2606:4700:4700::1111}
TCP_URL=${TCP_URL:-https://ifconfig.co/ip}
DNS_NAME=${DNS_NAME:-example.com}
IFACE=${IFACE:-${1:-}}

first_ipv6_cidr() {
    local iface=$1
    local addr
    local ip
    while read -r addr; do
        ip=${addr%%/*}
        case ${ip,,} in
            fc*|fd*)
                echo "$addr"
                return 0
                ;;
        esac
    done < <(ip -o -6 addr show dev "$iface" scope global | awk '{ print $4 }')
    ip -o -6 addr show dev "$iface" scope global | awk 'NR == 1 { print $4 }'
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

SRC_CIDR=${SRC_CIDR:-$(first_ipv6_cidr "$IFACE")}
SRC=${SRC:-${SRC_CIDR%%/*}}
if [[ -z $SRC ]]; then
    echo "Could not infer source IPv6 address on $IFACE. Set SRC=..." >&2
    exit 2
fi

if [[ -z ${NAT_GW:-} ]]; then
    if [[ -z $SRC_CIDR || $SRC_CIDR != */* ]]; then
        echo "Could not infer NAT66 gateway without source prefix. Set NAT_GW=..." >&2
        exit 2
    fi
    NAT_GW=$(SRC_CIDR=$SRC_CIDR python3 - <<'PY'
import ipaddress
import os

source = ipaddress.IPv6Interface(os.environ["SRC_CIDR"])
print(source.network.network_address + 1)
PY
)
fi

ROUTER=${ROUTER:-${GW:-}}
if [[ -z $ROUTER ]]; then
    ROUTER=$(ip -6 route show default dev "$IFACE" |
        awk '{ for (i = 1; i <= NF; ++i) if ($i == "via") { print $(i + 1); exit } }')
fi
if [[ -z $ROUTER && -z ${DST_MAC:-} ]]; then
    echo "Could not infer IPv6 next hop on $IFACE. Set ROUTER=... or DST_MAC=..." >&2
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
    ping -6 -c 1 -W 1 "${ROUTER}%${IFACE}" >/dev/null 2>&1 || true
    DST_MAC=$(ip -6 neigh show "$ROUTER" dev "$IFACE" |
        awk '{ for (i = 1; i <= NF; ++i) if ($i == "lladdr") { print $(i + 1); exit } }')
fi
if [[ -z $DST_MAC ]]; then
    echo "Could not infer destination MAC for $ROUTER on $IFACE. Set DST_MAC=..." >&2
    exit 2
fi

tmpdir=$(mktemp -d)
tcpdump_pid=

cleanup() {
    if [[ -n ${tcpdump_pid:-} ]] && kill -0 "$tcpdump_pid" >/dev/null 2>&1; then
        kill "$tcpdump_pid" >/dev/null 2>&1 || true
        wait "$tcpdump_pid" >/dev/null 2>&1 || true
    fi
    rm -rf "$tmpdir"
}
trap cleanup EXIT INT TERM

pass_count=0

section() {
    echo
    echo "== $* =="
}

pass() {
    echo "PASS: $*"
    ((++pass_count))
}

fail() {
    echo
    echo "FAIL: $*" >&2
    exit 1
}

start_tcpdump() {
    local log=$1
    tcpdump -i "$IFACE" -n -vv -l icmp6 >"$log" 2>&1 &
    tcpdump_pid=$!
    sleep 0.3
}

stop_tcpdump() {
    if [[ -n ${tcpdump_pid:-} ]] && kill -0 "$tcpdump_pid" >/dev/null 2>&1; then
        kill "$tcpdump_pid" >/dev/null 2>&1 || true
        wait "$tcpdump_pid" >/dev/null 2>&1 || true
    fi
    tcpdump_pid=
}

echo "iface=$IFACE"
echo "source_ipv6=$SRC"
echo "source_mac=$SRC_MAC"
echo "nat_gateway_ipv6=$NAT_GW"
echo "router_ipv6=${ROUTER:-unknown}"
echo "router_mac=$DST_MAC"
echo "target=$TARGET"
echo "tcp_url=$TCP_URL"
echo "dns_name=$DNS_NAME"

section "Gateway Echo"
if ping -6 -I "$IFACE" -c 1 -W 3 "$NAT_GW"; then
    pass "NAT66 gateway answers Echo locally."
else
    fail "NAT66 gateway Echo failed."
fi

section "Public Echo"
public_echo_log=$tmpdir/public-echo.tcpdump.log
start_tcpdump "$public_echo_log"
if ping -6 -I "$IFACE" -c 1 -W 3 "$TARGET"; then
    sleep 1
    stop_tcpdump
    echo
    echo "--- tcpdump public Echo capture ---"
    cat "$public_echo_log"
    if grep -F "$TARGET >" "$public_echo_log" | grep -qi 'echo reply'; then
        pass "public Echo Reply kept the upstream target as the apparent source."
    else
        fail "public Echo succeeded, but capture did not show Echo Reply from $TARGET."
    fi
else
    stop_tcpdump
    echo
    echo "--- tcpdump public Echo capture ---"
    cat "$public_echo_log"
    fail "public IPv6 Echo failed."
fi

section "Echo Hop Limit 1"
hop1_log=$tmpdir/echo-hop1.tcpdump.log
nping_log=$tmpdir/echo-hop1.nping.log
start_tcpdump "$hop1_log"

set +e
nping -6 --icmp --send-eth -N -e "$IFACE" \
    --source-mac "$SRC_MAC" --dest-mac "$DST_MAC" \
    -S "$SRC" --hop-limit 1 -c 1 --privileged "$TARGET" | tee "$nping_log"
nping_status=${PIPESTATUS[0]}
set -e

sleep 1
stop_tcpdump

echo
echo "--- tcpdump icmp6 capture ---"
cat "$hop1_log"

if grep -qi 'echo reply' "$hop1_log"; then
    echo "nping exit status: $nping_status"
    fail "observed an Echo Reply for the hop-limit-1 probe; the original Echo was not fully consumed."
fi

if ! grep -qi 'time exceeded' "$hop1_log"; then
    echo "nping exit status: $nping_status"
    fail "no ICMPv6 Time Exceeded was observed for the hop-limit-1 Echo probe."
fi
if ! grep -F "$NAT_GW >" "$hop1_log" | grep -qi 'time exceeded'; then
    fail "hop-limit-1 Time Exceeded was not sourced from the NAT66 gateway $NAT_GW."
fi
pass "hop-limit-1 Echo produced gateway-sourced Time Exceeded and no Echo Reply."

section "Echo Hop Limit 2"
set +e
hop2_output=$(ping -6 -I "$IFACE" -t 2 -c 1 -W 3 "$TARGET" 2>&1)
hop2_status=$?
set -e
echo "$hop2_output"
if ! grep -qi 'time exceeded' <<<"$hop2_output"; then
    echo "ping exit status: $hop2_status"
    fail "hop-limit-2 Echo did not produce Time Exceeded."
fi
pass "hop-limit-2 Echo produced best-effort upstream Time Exceeded."

section "UDP DNS"
IFACE=$IFACE SRC=$SRC TARGET=$TARGET DNS_NAME=$DNS_NAME python3 - <<'PY'
import os
import socket
import time

iface = os.environ["IFACE"]
source = os.environ["SRC"]
target = os.environ["TARGET"]
name = os.environ["DNS_NAME"].rstrip(".")

labels = name.split(".")
qname = b"".join(bytes([len(label)]) + label.encode("ascii") for label in labels) + b"\0"
query = b"\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00" + qname + b"\x00\x01\x00\x01"

sock = socket.socket(socket.AF_INET6, socket.SOCK_DGRAM)
sock.setsockopt(socket.SOL_SOCKET, socket.SO_BINDTODEVICE, iface.encode() + b"\0")
sock.bind((source, 0, 0, socket.if_nametoindex(iface)))
sock.settimeout(3)
started = time.monotonic()
sock.sendto(query, (target, 53, 0, 0))
data, addr = sock.recvfrom(4096)
elapsed_ms = round((time.monotonic() - started) * 1000, 1)
if len(data) < 12 or data[:2] != b"\x12\x34":
    raise SystemExit(f"unexpected DNS reply from {addr}: {data.hex()}")
print(f"UDP DNS reply {len(data)} bytes from {addr[0]}:{addr[1]} in {elapsed_ms} ms")
PY
pass "UDP DNS works through NAT66."

section "UDP Hop Limit Errors"
IFACE=$IFACE SRC=$SRC TARGET=$TARGET NAT_GW=$NAT_GW python3 - <<'PY'
import os
import select
import socket
import struct
import time

iface = os.environ["IFACE"]
source = os.environ["SRC"]
target = os.environ["TARGET"]
gateway = os.environ["NAT_GW"]

IPV6_RECVERR = 25
MSG_ERRQUEUE = 0x2000
SO_EE_ORIGIN_ICMP6 = 3
ICMPV6_TIME_EXCEEDED = 3


def parse_error(cdata):
    ee_errno, ee_origin, ee_type, ee_code, ee_pad, ee_info, ee_data = struct.unpack_from("=I4BII", cdata)
    offender = None
    if len(cdata) >= 44 and struct.unpack_from("H", cdata, 16)[0] == socket.AF_INET6:
        offender = socket.inet_ntop(socket.AF_INET6, cdata[24:40])
    return ee_errno, ee_origin, ee_type, ee_code, ee_info, ee_data, offender


def expect_time_exceeded(hops, port):
    sock = socket.socket(socket.AF_INET6, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_BINDTODEVICE, iface.encode() + b"\0")
    sock.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_UNICAST_HOPS, hops)
    sock.setsockopt(socket.IPPROTO_IPV6, IPV6_RECVERR, 1)
    sock.bind((source, 0, 0, socket.if_nametoindex(iface)))
    sock.setblocking(False)
    sock.sendto(f"vpnhotspot-hop-limit-{hops}".encode(), (target, port, 0, 0))
    deadline = time.monotonic() + 4
    while True:
        timeout = deadline - time.monotonic()
        if timeout <= 0:
            raise SystemExit(f"timeout waiting for UDP hop-limit-{hops} error queue")
        select.select([sock], [], [], timeout)
        try:
            _data, ancillary, flags, addr = sock.recvmsg(4096, 4096, MSG_ERRQUEUE)
        except BlockingIOError:
            continue
        for level, typ, cdata in ancillary:
            if level != socket.IPPROTO_IPV6 or typ != IPV6_RECVERR or len(cdata) < 16:
                continue
            ee_errno, ee_origin, ee_type, ee_code, ee_info, ee_data, offender = parse_error(cdata)
            if ee_origin == SO_EE_ORIGIN_ICMP6 and ee_type == ICMPV6_TIME_EXCEEDED and ee_code == 0:
                print(
                    f"UDP hop-limit-{hops} Time Exceeded from {offender} "
                    f"for {addr[0]}:{addr[1]} flags={flags} errno={ee_errno} info={ee_info} data={ee_data}"
                )
                return offender
        raise SystemExit(f"UDP hop-limit-{hops} produced no matching ICMPv6 Time Exceeded")


hop1 = expect_time_exceeded(1, 33434)
if hop1 != gateway:
    raise SystemExit(f"hop-limit-1 offender {hop1} != gateway {gateway}")

hop2 = expect_time_exceeded(2, 33435)
if hop2 is None:
    raise SystemExit("hop-limit-2 offender missing")
PY
pass "UDP hop-limit errors are translated through the IPv6 error queue."

section "TCP"
if curl -6 --interface "$IFACE" --max-time 10 "$TCP_URL"; then
    pass "TCP works through NAT66."
else
    fail "TCP smoke test failed for $TCP_URL."
fi

echo
echo "PASS: $pass_count NAT66 ICMP/UDP/TCP smoke checks passed."
