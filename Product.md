# Vyse — Product Spec

## 1. Vision & Market Positioning

**Project Name:** Vyse  
**Tagline:** A fast, reconnect-proof tunnel for local dev that gives you persistent URLs, multi-port routing, and instant webhook replays — built on modern QUIC.  
**Target Audience:** Full-stack developers, infrastructure engineers, and enterprise platform teams.

Legacy tunneling tools like ngrok and localtunnel are built on aging HTTP/1.1 and TCP architectures. They suffer from head-of-line blocking, connection drops on network switching, poor UDP support, and no way to inspect or sanitize traffic before it hits `localhost`.

**Vyse** is a reconnect-proof ephemeral tunnel. The public edge speaks **HTTP/3**. The laptop connection is **QUIC**, so a Wi-Fi → cellular switch keeps the same URL and session. One command exposes multiple local ports. Incoming webhooks are captured locally so they can be inspected and replayed without a third-party dashboard.

Wasm plugins, eBPF interception, and MASQUE datagram proxying are **add-ons**. They are not required to ship the product described above.

## 2. What ships vs what waits

### v1 — The product (ship this)

This is the contract in the tagline. Do not cut it down to a thinner “MVP.”

| Capability | Why it is v1 |
| --- | --- |
| **QUIC tunnel** (CLI ↔ edge) with keepalives, multiplexed streams, and connection migration | This is the reconnect-proof promise. |
| **HTTP/3 on the public edge** | This is the advertised public protocol. The edge accepts HTTP/3 (QUIC, ALPN `h3`) and translates to HTTP/1.1 toward the local app. |
| **HTTP/1.1 compatibility listener** | Not a substitute for HTTP/3. Stripe, GitHub, Slack, and most webhook senders still speak HTTP/1.1. Every production HTTP/3 stack keeps this fallback. |
| **Persistent URLs** | Dynamic subdomain registration (`my-app.vyse.dev`) with stream multiplexing onto one QUIC session. |
| **Multi-port routing** | `vyse tunnel --route "/api=8000" --route "/=3000"` — one URL, several local services, no sidecars. |
| **Webhook Studio** | Local SQLite log (last 500 requests), inspect, `vyse replay <id>`, and a TUI while the tunnel is live. |
| **`vyse` CLI** | `tunnel` and `replay` are the two commands the product is about. |

HTTP/1.1 on the public side exists so the product works with the real internet. It does not replace HTTP/3.

### Later — Platform add-ons

Ship after v1. Do not block the first version on these.

| Add-on | Role |
| --- | --- |
| **MASQUE (RFC 9298) UDP datagrams** | HTTP/3 CONNECT-UDP to registered local UDP ports. Raw public UDP still later. |
| **Wasm edge middleware** | In-flight PII redaction, header injection, JWT/OIDC at the edge. |
| **eBPF `sock_ops`** | Kernel-level local routing without binding extra ports. |
| **Distributed control plane** | Raft/Redis registry, API tokens, multi-node edge. |
| **SPIFFE/SPIRE** | Enterprise identity so traffic never hits a shared multi-tenant relay. |
| **Hosted `*.vyse.dev` + production TLS** | Cloudflare wildcard DNS and real certificates. Code supports it; provisioning is ops. |

## 3. Core technology (v1 vs add-on)

| Technology | Role | When |
| --- | --- | --- |
| **QUIC** | CLI ↔ edge transport, connection migration, stream multiplexing | v1 |
| **HTTP/3** | Public-facing protocol at the edge | v1 |
| **HTTP/1.1** | Public compatibility + local-app dialect (almost every `localhost` server) | v1 |
| **MASQUE** | HTTP/3 CONNECT-UDP to advertised local UDP ports | shipped (add-on) |
| **eBPF (`sock_ops`)** | Sidecar-less kernel redirection | add-on |
| **WebAssembly** | Sandboxed edge middleware | add-on |

## 4. System Architecture

Three components. All v1 code is Rust.

### A. Vyse Edge (cloud relay) — v1

* **Public HTTP/3:** QUIC listener, ALPN `h3`. Route by `:authority` / `Host` to an active tunnel, then by path to a registered local port.
* **Public HTTP/1.1:** TCP listener for clients that cannot speak HTTP/3.
* **Tunnel QUIC:** Separate ALPN (`vyse`) for the CLI daemon. One connection, many bidirectional streams.
* **Add-on later:** Wasm middleware in this process.

### B. Control plane — v1 vs add-on

* **v1:** In-process registry of subdomain → QUIC connection + route table.
* **Add-on later:** Raft/Redis, API tokens, SPIFFE/SPIRE.

### C. Vyse CLI (local daemon) — v1

* Dials the edge over QUIC and holds a multiplexed control channel.
* Forwards each tunneled request to the matching local port.
* Webhook Studio: SQLite + TUI + `vyse replay`.
* **Add-on later:** optional eBPF probes (requires root).

```text
Public client
    │  HTTP/3 (QUIC)     HTTP/1.1 (TCP fallback)
    ▼
Vyse Edge
    │  QUIC ALPN "vyse"  (connection migration)
    ▼
Vyse CLI  →  localhost:3000 / :8000 / …
             SQLite request log
```

## 5. v1 checklist

### Transport & URLs

* [x] QUIC server and client (`quinn`), multiplexed streams, keepalives
* [x] HTTP/3 public listener (ALPN `h3`) translating to the local HTTP/1.1 app
* [x] HTTP/1.1 public compatibility listener
* [x] Dynamic subdomain registration
* [x] QUIC 0-RTT / connection migration enabled and documented
* [ ] Hosted `*.vyse.dev` DNS (ops; documented, not a code blocker)

### Developer product

* [x] `vyse serve 3000` (first-run subdomain prompt; hardware-id binding until the dashboard)
* [x] Local SQLite logger (last 500 requests)
* [x] `vyse replay <req_id>` against localhost
* [x] Live TUI for captured webhooks while the tunnel is running

### Explicitly not v1

* [x] MASQUE UDP (RFC 9298 CONNECT-UDP on HTTP/3, registered local ports only)
* [ ] Wasmtime edge plugins
* [ ] eBPF `sock_ops`
* [ ] Raft/Redis / SPIFFE
