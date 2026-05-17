# DNS

The DNS runtime is per session. It owns daemon listeners for downstream DNS and
the handoff to Android's resolver API. Routing owns how downstream packets reach
those listeners.

## Listener Ownership

[`dns::Runtime::start`](../../mobile/src/main/rust/vpnhotspotd/src/dns.rs)
attempts to bind TCP and UDP listeners to the session's downstream IPv4 address
on ephemeral ports. Each listener is an independent best-effort capability. A
listener that starts publishes its port to routing; a listener that fails is
reported as a structured nonfatal tied to the start-session call and omitted
from routing.

Routing redirects downstream IPv4 DNS to the ephemeral ports that exist with
DNAT. NAT66 TCP and UDP also special-case DNS to the NAT66 gateway on port 53
and call the same DNS handlers. This keeps resolver selection and DNS response
generation in one runtime instead of duplicating DNS behavior in NAT66.

## Config Snapshots

The DNS runtime receives the shared session config. For each query or TCP
connection, it clones a snapshot before resolving. That snapshot determines the
primary and fallback Android networks used for resolver calls.

Current selection is simple:

1. use `primary_network` when present;
2. otherwise use `fallback_network` when present;
3. otherwise return a DNS failure.

DNS does not hold the config mutex while waiting for Android resolver results.

## Resolver Handoff

DNS queries are sent through bionic's Android resolver API:

- `android_res_nsend` starts a one-shot query on the selected Android network;
- `android_res_nresult` reads and closes the result;
- `android_res_cancel` is used if the query object is dropped before finish.

`android_res_nsend` returns a file descriptor. The daemon sets it nonblocking
and wraps it in `AsyncFd`. The daemon waits for the resolver-side socket to
become readable and then for EOF before calling `android_res_nresult`.

This shape exists because `android_res_nresult` is the public result
reader/closer. The root README records the platform assumption: the resolver
service writes the complete result before returning and closes the socket when
the result is ready, so the daemon can stay nonblocking while still using the
public result API.

## TCP DNS

TCP DNS accepts normal DNS-over-TCP framing:

- read a two-byte length;
- read exactly that many query bytes;
- resolve the query through the selected Android network;
- write a two-byte response length and response bytes.

Unexpected EOF while reading the next frame ends the connection cleanly. Other
I/O failures are returned to the connection task.

## UDP DNS

UDP DNS reads one datagram, clones the config snapshot, and resolves the query
in a child task. If resolution succeeds, the response is sent back to the
datagram source. Send failures are reported with source context.

The UDP listener intentionally does not serialize all queries through one
worker. Each query has its own child task tied to the session stop token.

## Failure Semantics

Resolver failure normally returns a SERVFAIL response when the query can be
parsed enough to build one. If a SERVFAIL response cannot be generated, the
query is dropped.

Per-query resolver failures are logged but do not stop the DNS runtime. Listener
setup failures do not stop the session. Routing omits the missing DNS redirect,
so normal IP traffic and manually configured downstream DNS can still work.
