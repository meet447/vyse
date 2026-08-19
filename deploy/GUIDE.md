# Deploy Vyse to the OCI VPS + vyse.chipling.xyz

Public IP: `144.24.98.146`  
Apex: `vyse.chipling.xyz`  
Tunnels: `https://<subdomain>.vyse.chipling.xyz`

The edge binary and Caddy config are already on the VPS. Two things still have to be opened in cloud consoles: **Cloudflare DNS** and **OCI UDP 4433**.

## 1. Cloudflare DNS (required)

`chipling.xyz` already uses Cloudflare (`brit.ns.cloudflare.com` / `pete.ns.cloudflare.com`).

**Cloudflare Dashboard → chipling.xyz → DNS → Add record** (twice):

| Type | Name | IPv4 address | Proxy status |
| --- | --- | --- | --- |
| A | `vyse` | `144.24.98.146` | **DNS only** (grey cloud) |
| A | `*.vyse` | `144.24.98.146` | **DNS only** (grey cloud) |

Leave the proxy **off**. Orange-cloud would send `vyse.chipling.xyz:4433` to Cloudflare, and the QUIC tunnel would never reach the VPS.

Check:

```bash
dig +short vyse.chipling.xyz A
# must print 144.24.98.146

dig +short hello.vyse.chipling.xyz A
# must print 144.24.98.146
```

## 2. Oracle Cloud security list (required)

In the OCI console, open the **VCN → Security List / NSG** attached to this instance. Add **Ingress**:

| Source | IP protocol | Destination port |
| --- | --- | --- |
| 0.0.0.0/0 | UDP | **4433** |

TCP 80/443 and UDP 443 are already in use by Caddy (including `api.koraku.chipling.xyz`), so those are likely already allowed. UDP **4433** is the new one. Without it, `vyse tunnel` from your laptop will hang.

On the VM, iptables already has `ACCEPT udp dpt:4433`.

## 3. What is already running

| Bind | Process | Role |
| --- | --- | --- |
| TCP 80 / 443, UDP 443 | Caddy | Public HTTPS + HTTP/3, Let's Encrypt |
| `127.0.0.1:8080` | `vyse-edge` | HTTP, only Caddy talks to it |
| UDP `0.0.0.0:4433` | `vyse-edge` | QUIC tunnel for the CLI |

`api.koraku.chipling.xyz` is unchanged in the same Caddyfile.

The first visit to a new hostname (`https://my-app.vyse.chipling.xyz`) issues an on-demand Let's Encrypt cert. That first request can take 10–30 seconds.

## 4. Use it from your laptop

```bash
python3 -m http.server 3000

cargo run -p vyse-cli -- tunnel \
  --edge vyse.chipling.xyz:4433 \
  --subdomain my-app \
  --port 3000
```

Then open `https://my-app.vyse.chipling.xyz`.

Until DNS exists you can still test the tunnel with the raw IP:

```bash
cargo run -p vyse-cli -- tunnel \
  --edge 144.24.98.146:4433 \
  --server-name localhost \
  --subdomain my-app \
  --port 3000
```

Public HTTPS still needs the Cloudflare records above.

## 5. Service commands

```bash
ssh -i /Users/meetsonawane/Desktop/oci/ssh-key-2025-12-14.key ubuntu@144.24.98.146

sudo systemctl status vyse-edge
sudo journalctl -u vyse-edge -f
sudo systemctl restart vyse-edge
sudo systemctl reload caddy
```

## 6. Redeploy a new binary

From the repo on your Mac:

```bash
docker run --rm --platform linux/amd64 \
  -v "$PWD":/src -w /src \
  -e CARGO_TARGET_DIR=/src/target-linux \
  rust:1-bookworm \
  cargo build --release -p vyse-edge

scp -i /Users/meetsonawane/Desktop/oci/ssh-key-2025-12-14.key \
  target-linux/release/vyse-edge \
  ubuntu@144.24.98.146:/tmp/vyse-edge

ssh -i /Users/meetsonawane/Desktop/oci/ssh-key-2025-12-14.key ubuntu@144.24.98.146 \
  'sudo install -m 755 /tmp/vyse-edge /usr/local/bin/vyse-edge && sudo systemctl restart vyse-edge'
```
