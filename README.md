# Travel router (Raspberry Pi)

Replaces RaspAP. Two layers, deliberately separated:

| Layer | Owns | Changes | Tool |
|---|---|---|---|
| **IaC** | what the box *is* — packages, hostapd, dnsmasq base, `netmode` itself | rarely | Ansible |
| **Runtime** | what role `eth0` plays *right now* | per venue | `netmode` |

Runtime mode is not expressed as a git commit, because switching modes must work
when you have no network — which is exactly when you need to switch.

## Hardware invariants

These are physical facts, not settings:

- `wlan0` (built-in brcmfmac) — **always** a wifi client; it cannot AP reliably
- `wlan1` (USB MT7921)        — **always** the AP
- `eth0`                      — the only interface that changes role

So there are exactly two modes.

Modes are named for **which way `eth0` faces**, never for whose WAN it is —
"eth0 is the WAN for UniFi" means eth0 *serves*, so perspective-based names
invite exactly the wrong command.

| | `eth0` | uplink | serving |
|---|---|---|---|
| `netmode serve` | **provides** a WAN interface to another client (UniFi) | `wlan0` → hotspot | `eth0` (10.6.141.1) + `wlan1` AP (10.9.141.1) |
| `netmode uplink` | **connects to** a WAN (venue ethernet, DHCP) | `eth0` | `wlan1` AP only |

`lan` and `wan` still work as aliases for `serve` and `uplink`.

## Use

    netmode serve      # eth0 provides a WAN to UniFi (default)
    netmode uplink     # eth0 consumes a WAN from the venue
    netmode status     # declared vs actual
    netmode converge   # re-apply declared mode (runs at boot)

`serve`/`uplink` write `/etc/netmode/mode`, then converge. Boot runs the same
converge path, so there is no drift between "what I asked for" and "what boots".

## Deploy

### First run, or any run on a box you cannot physically reach

Stage it. Install the files without applying anything live, then bring the
firewall up behind a rollback window:

    cd ansible

    # 1. files only - netmode, dnsmasq snippets, boot unit. Nothing applied.
    ansible-playbook site.yml --ask-become-pass -e converge_on_deploy=false

    # 2. apply mode + firewall, auto-reverting in 5 minutes
    ssh -t pi@10.6.141.1 'sudo netmode serve --rollback 300'

    # 3. from a NEW terminal - prove you can still get in, then keep it
    ssh -t pi@10.6.141.1 'sudo netmode confirm'

Step 3 must be a **new** session. `ct state established` keeps an existing
connection alive straight through a broken ruleset, so confirming from the
window you switched from proves nothing.

### Routine runs

    ansible-playbook site.yml --ask-become-pass

This DOES converge — it applies the mode and firewall immediately, with no
rollback window. Only appropriate once the ruleset is known good.

### Flags

| flag | default | effect |
|---|---|---|
| `converge_on_deploy` | `true` | apply mode+firewall at the end of the run |
| `purge_raspap` | `false` | stop and disable the `raspapd` daemon and `lighttpd` web UI |

RaspAP's **conflicting dnsmasq fragments are always removed**, regardless of
`purge_raspap` — they are not optional. Verified with `dnsmasq --test`:
`090_raspap.conf` and our `10-base.conf` both set `log-facility`, which dnsmasq
rejects outright as `illegal repeated keyword`, and `090_wlan1.conf` duplicates
our `wlan1` dhcp-range. Left in place, dnsmasq refuses to start at all.
`090_adblock.conf` and `099-upstream.conf` don't conflict and are left alone —
they are also inert now that dnsmasq no longer answers DNS at all.

`ap_passphrase` is undefined on purpose, so `hostapd.conf` is left alone and
your AP keeps its current config and clients. Vault it to manage the AP:

    ansible-vault create group_vars/pi_router/vault.yml

### Expect during a converge

`netmode` restarts `dhcpcd` for `eth0`, which briefly bounces the wired link —
and with it the WAN of anything downstream. Harmless, but not silent.

## Firewall

nftables, one table `inet netmode`, regenerated on every mode switch and
validated with `nft -c` before it is ever loaded.

    input    policy drop  - established/related, loopback, some ICMP,
                            DHCP+DNS from LAN legs, SSH from admin legs
    forward  policy drop  - LAN -> egress, LAN <-> LAN, replies inbound
    output   policy drop  - ONLY when the profile requires a tunnel: wg0, LAN,
                            loopback, the WireGuard handshake, DHCP renewal
    nat      masquerade on the current egress

Nothing is accepted inbound on the uplink. On a travel router the uplink is
hostile by definition.

## Admin plane

SSH is permitted only on interfaces acting as **LAN in the current mode** —
role-based, not name-based, so it follows `eth0` when it changes job:

| mode | admin allowed | denied |
|---|---|---|
| `serve` | `wlan1` AP, `eth0` | `wlan0` (uplink) |
| `uplink` | `wlan1` AP | `eth0` (now uplink), `wlan0` |

`admin_on_uplink: true` overrides this and exposes SSH to the venue. It defaults
to false and should stay there.

### Don't lock yourself out

Tightening this remotely can strand you at a conference. Use the rollback window:

    netmode serve --rollback 300    # firewall reverts in 5 min unless confirmed
    netmode confirm               # cancel the revert, keep the ruleset

Existing SSH sessions survive a bad ruleset (the `ct state established` rule is
matched first) — it's the *next* connection that would fail. Confirm from a
**new** session, not the one you switched from.

## DNS

**blocky owns `:53`. dnsmasq is a DHCP server only** (`port=0`).

- Blocklist is **security-only** — malware, phishing, ransomware, scam,
  cryptojacking (HaGeZi TIF Medium, ~360k domains). Ads, adult and gambling are
  deliberately *not* blocked; ad blocking happens on-device.
- Every upstream query leaves over **DoH**, in every profile. Quad9 first
  (it filters malicious domains itself, so it is a second opinion rather than a
  duplicate of the list), Cloudflare as the fallback for venues that block it.
- Quad9 is given by hostname because **its certificate has no IP SANs**.
  Bootstrapping it needs an IP-addressed DoH endpoint, which is what
  `blocky_bootstrap` (`https://1.1.1.1/dns-query`) is for. There is no
  plaintext DNS path anywhere, including bootstrap.
- Clients are handed **this box and nothing else** by DHCP option 6. A public
  secondary would be a standing bypass around the blocklist, the DoH upstream
  and the kill switch.
- The box resolves through blocky too — `/etc/resolv.conf` is pinned to
  `127.0.0.1`, and `DNS =` is stripped from `wg0.conf` so wg-quick cannot
  point it at Proton behind blocky's back.
- Listen addresses are **specific, never `0.0.0.0`**. The input chain already
  refuses `:53` from the uplink, but `netmode rollback-fire` deletes that whole
  table — a wildcard bind would turn the lockout insurance into an open
  resolver on the venue's wifi. `netmode` rewrites `20-listen.yml` per mode.

**Known loss:** DHCP lease names no longer resolve. The resolver that knows the
leases is no longer the resolver clients ask.

### DoH listener

**Live.** `https://10.9.141.1/dns-query` and `https://10.6.141.1/dns-query`,
TLS 1.3, LAN legs only.

The trust anchor is a **dedicated root made for this box alone**
(`bin/make-doh-cert.sh --new-root`) — deliberately not chained to any estate or
employer CA. This router is carried into hostile networks; the anchor that
vouches for it should vouch for nothing else. Root and intermediate keys live
on the controller in `~/.rasputin-pki` and never reach the router; the leaf key
is the only private key on the box.

Install `~/.rasputin-pki/root-ca.crt` on each device that uses the AP.

`blocky_doh_cert`/`blocky_doh_key` are **not** set in defaults. The role sets
them only after finding the material on the controller, because a blocky
pointed at a certificate that does not exist fails to start and there is no
second resolver here.

The leaf carries **IP SANs for both LAN legs**. That is load-bearing: a client
asking this box to resolve names cannot resolve the box's own name first, so
DoH has to work as `https://10.9.141.1/dns-query`. `certFile` is the
**fullchain** — blocky serves exactly what it is given and will not send the
intermediate on its own.

Both certs are long-lived on purpose. This box cannot reach a CA from a hotel,
and a DoH listener whose certificate expired mid-trip is a router with no DNS.

Opening the listener is **two** changes, not one: binding the port and opening
it in the input chain. netmode adds `tcp dport 443` for LAN legs only when the
cert vars are defined. Without that rule the port is bound but every client
sees a dead connection — which is exactly how this first failed.

Verify from a client:

    curl --cacert ~/.rasputin-pki/root-ca.crt \
      -H 'accept: application/dns-message' \
      "https://10.6.141.1/dns-query?dns=AAABAAABAAAAAAAAA3d3dwdleGFtcGxlA2NvbQAAAQAB"

### Verifying the kill switch

**Both halves are proven on hardware.** Forward: dropping `wg0` gave clients
100% loss and zero leak. Output, verified 2026-08-23 on `hotel-wifi` with the
tunnel destroyed by `ip link del wg0`:

- blocky's upstream connections sat in **`SYN-SENT` to `1.1.1.1:443`** from the
  venue address — the SYNs were generated and never left.
- a counter rule appended to the output chain caught **38 packets / 2280 bytes**
  falling through to `policy drop` on the uplink.
- **no** ESTABLISHED connection to any resolver.
- a client query returned `no servers could be reached`. DNS stopped rather
  than resolving via the venue.

To re-run it, arm the auto-revert first — this drops all client traffic:

    sudo systemd-run --unit=netmode-safety --on-active=600 /usr/local/sbin/netmode home
    sudo systemd-run --unit=netmode-apply --collect /usr/local/sbin/netmode hotel-wifi
    netmode status                      # expect: output guard : policy drop

    # instrument the drop path, then kill the tunnel
    sudo nft add rule inet netmode output oifname \"wlan0\" counter comment \"leaktest\"
    sudo ip link del wg0
    sudo systemctl restart blocky       # force FRESH upstream connections
    sudo nft list chain inet netmode output | grep leaktest

    sudo /usr/local/sbin/netmode home   # converge also wipes the counter rule

**`blocky healthcheck` still returns OK with the tunnel dead** — it only proves
blocky is listening on `127.0.0.1:53`, not that it can resolve anything. Use an
uncached client query to test upstream reachability.

`dig` is **not installed on the Pi** — query from a client.

### Kill switch

Two halves, and the second one is new:

- **forward** — clients cannot egress except via `wg0`. Proven end-to-end:
  dropping `wg0` by hand gave 100% loss and zero leak.
- **output** — *this box* cannot egress except via `wg0` either. Needed the
  moment the resolver's upstream became a public DoH endpoint: previously the
  upstream was `10.2.0.1`, routable only inside the tunnel, so a dead `wg0`
  killed the router's own DNS by routing alone. That accident is gone, and
  this rule replaces it. Disable with `killswitch_restrict_output: false`.

  There is no bootstrap deadlock: the WireGuard peer is a literal IP, so the
  tunnel needs neither DNS nor a correct clock. If the peer endpoint cannot be
  read the chain **fails open with a loud warning** rather than stranding the
  box — `netmode status` reports whether it is armed.

## Two guard rails, both earned

1. **Validate before restart.** Handlers run `dnsmasq --test` first. RaspAP wrote
   an invalid fragment, dnsmasq died, and its only recovery action was `reload` —
   which cannot revive a failed unit, so it threw a PHP stack trace forever.
2. **No default route out a LAN interface.** `netmode` asserts this after every
   converge and refuses to finish otherwise. RaspAP wrote `static routers=10.6.141.1`
   on `eth0` — a gateway pointing at the box itself — which black-holed every
   forwarded packet while the WAN was perfectly healthy.

## Known separate issues

- `avahi-daemon` is enabled and announces this box on whatever network it can
  reach. On a conference uplink that is an inventory leak. Not addressed here.
- Captive portals at hotels/conferences still need a browser on the WAN side or
  a cloned MAC. Not solved here.
