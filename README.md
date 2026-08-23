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
`090_adblock.conf` and `099-upstream.conf` don't conflict and are left alone.

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
    forward  policy drop  - LAN -> uplink, LAN <-> LAN, replies inbound
    nat      masquerade on the current uplink

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

- dnsmasq serves DHCP+DNS on LAN legs only. `bind-dynamic` +
  `except-interface` are load-bearing: `interface=` alone leaves a wildcard
  socket open, which would make this an open resolver on hotel wifi.
- Clients are handed public resolvers by DHCP option 6, so they resolve even
  when this box's own upstream is broken.
- The box's own upstream is still `099-upstream.conf` — unmanaged, see below.

## Two guard rails, both earned

1. **Validate before restart.** Handlers run `dnsmasq --test` first. RaspAP wrote
   an invalid fragment, dnsmasq died, and its only recovery action was `reload` —
   which cannot revive a failed unit, so it threw a PHP stack trace forever.
2. **No default route out a LAN interface.** `netmode` asserts this after every
   converge and refuses to finish otherwise. RaspAP wrote `static routers=10.6.141.1`
   on `eth0` — a gateway pointing at the box itself — which black-holed every
   forwarded packet while the WAN was perfectly healthy.

## Known separate issues

- `099-upstream.conf` sets `no-resolv` + `server=10.2.0.1` (Proton, inside the
  WireGuard tunnel) and **there is no WireGuard interface**. Fail-closed DNS that
  is currently just failing. Left untouched — it's a policy decision, not a bug.
  DHCP clients are handed public resolvers directly so they don't depend on it.
- Captive portals at hotels/conferences still need a browser on the WAN side or
  a cloned MAC. Not solved here.
