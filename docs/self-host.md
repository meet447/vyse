# Self-hosting Vyse

Run your own Vyse edge so the CLI tunnels through infrastructure you control instead of the hosted service at `vyse.chipling.xyz`.

## Prerequisites

- Rust stable toolchain
- A machine reachable from clients that will hit your public HTTP listeners
- Local ports available for QUIC, HTTP/1.1, and HTTP/3

## Start the edge

From the repository root:

```bash
cargo run -p vyse-edge -- \
  --quic 0.0.0.0:4433 \
  --http 127.0.0.1:8080 \
  --http3 127.0.0.1:8443 \
  --domain localhost \
  --public-base http://localhost:8080
```

| Flag | Role |
| --- | --- |
| `--quic` | QUIC listener for CLI tunnel connections (ALPN `vyse`) |
| `--http` | Public HTTP/1.1 compatibility listener |
| `--http3` | Public HTTP/3 listener (ALPN `h3`) |
| `--domain` | Apex domain used for `Host`-based routing |
| `--public-base` | Origin URL returned to the CLI after registration |

The edge uses self-signed TLS for QUIC and HTTP/3 in local development.

## Point the CLI at your edge

In another terminal, forward a local app through your edge:

```bash
vyse serve 3000 \
  --edge 127.0.0.1:4433 \
  --server-name localhost \
  --subdomain my-app
```

The `--edge`, `--server-name`, and `--subdomain` flags are hidden in `--help` but available for self-hosting and integration tests.

Your public URL will look like:

```text
http://my-app.localhost:8080
```

Use `--route` for multi-port routing on one URL (same as the hosted CLI):

```bash
vyse serve 3000 --route "/api=8000" --route "/=3000" \
  --edge 127.0.0.1:4433 --server-name localhost --subdomain my-app
```

Advertise a local UDP port for RFC 9298 CONNECT-UDP on the HTTP/3 listener:

```bash
vyse serve 3000 --udp 5353 \
  --edge 127.0.0.1:4433 --server-name localhost --subdomain my-app
```

MASQUE clients connect to:

```text
https://my-app.localhost/.well-known/masque/udp/127.0.0.1/5353/
```

The edge only forwards to loopback (`127.0.0.1`, `localhost`, `::1`) on ports listed in `--udp`.

## Persistent subdomain claims (optional)

By default, subdomain ownership is in-memory only. To persist reserved names across edge restarts, pass a SQLite path:

```bash
cargo run -p vyse-edge -- \
  --quic 0.0.0.0:4433 \
  --http 127.0.0.1:8080 \
  --http3 127.0.0.1:8443 \
  --domain localhost \
  --public-base http://localhost:8080 \
  --claims ./claims.db
```

When `--claims` is set, the edge requires the CLI to send a stable `machine_id` (the CLI generates and stores one automatically). A subdomain is bound to the first machine that claims it.

## Crates

| Crate | Binary | Purpose |
| --- | --- | --- |
| `vyse-core` | — | Shared protocol, QUIC helpers, routing |
| `vyse-edge` | `vyse-edge` | Public gateway and tunnel registry |
| `vyse-cli` | `vyse` | Local daemon, webhook log, TUI |

## Production notes

For a real deployment you will need:

- DNS pointing `*.yourdomain` at your edge
- TLS certificates trusted by browsers (the dev edge uses self-signed certs)
- Firewall rules for your chosen QUIC/HTTP ports

Production-specific provisioning for the hosted service is maintained outside this repository (see [CONTRIBUTING.md](../CONTRIBUTING.md)).
