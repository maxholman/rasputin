# Travel router (Raspberry Pi)

Replaces RaspAP. Two layers, deliberately separated:

| Layer | Owns | Changes | Tool |
|---|---|---|---|
| **IaC** | what the box *is* — packages, hostapd, dnsmasq base, `rasputin` itself | rarely | Ansible |
| **Runtime** | what role `eth0` plays *right now* | per venue | `rasputin` |

Runtime mode is not expressed as a git commit, because switching modes must work
when you have no network — which is exactly when you need to switch.

## Hardware invariants

These are physical facts, not settings:

- `wlan0` (built-in brcmfmac) — **always** a wifi client; it cannot AP reliably
- `wlan1` (USB MT7921)        — **always** the AP
- `eth0`                      — the only interface that changes role

So a **profile** has two axes and nothing else: which leg faces the venue, and
whether anything may leave except through the tunnel. `eth0`'s role follows
from the first — a DHCP client when it *is* the uplink, a LAN leg (10.6.141.1,
DHCP server) otherwise — and an unplugged LAN leg has nobody to serve, so it
stays out of the way.

| profile | uplink | tunnels + kill switch | `eth0` |
|---|---|---|---|
| `home` | `wlan0` → own hotspot | none | serves the wired net |
| `hotel-wifi` | `wlan0` → venue wifi | **one**: `wg0` carries everything | serves the wired net |
| `hotel-split` | `wlan0` → venue wifi | **two**: `wg0` for everything, `wgsg` for the split domains | serves the wired net |
| `hotel-wifi-novpn` | `wlan0` → venue wifi | none | serves the wired net |
| `hotel-eth` | `eth0` → venue ethernet | **one**: `wg0` carries everything | is the uplink |
| `hotel-eth-novpn` | `eth0` → venue ethernet | none | is the uplink |

A second tunnel because one exit is rarely right for everything: a near exit is
fast for the bulk of the traffic, and a far one is for the services that
refuse the near one (from Hong Kong, that is Claude and OpenAI). The split is
by domain, the way UniFi does it: blocky forwards `split_domains` to dnsmasq,
dnsmasq answers them from the resolver *inside* the far tunnel and records every
address in an nftables set, and a mangle rule marks packets to those addresses
into a routing table whose only route is that tunnel. Nothing is ever routed in
the clear, so the kill switch is exactly as tight as with one tunnel. A client
running its own DoH is not classified and exits with everything else. Each
tunnel is a Proton WireGuard config in `/etc/wireguard/<name>.conf`; the role
never carries those keys, so they are copied onto the box by hand.

Both tunnels come off one Proton account, so they share an address (`10.2.0.2`)
and a resolver (`10.2.0.1`). That is survivable, but only because `split_start`
widens the anti-spoof rule wg-quick installs for the default tunnel — untouched,
that rule drops every reply the second tunnel receives, and the symptom is a
tunnel that handshakes in two seconds and never carries a byte. The default
tunnel must be an exit where the split domains still work: every fallback path
lands there.

There is no mode word to type. `serve` and `uplink` used to say which way
`eth0` faced and nothing about the tunnel, which is how the box spent a night
on venue wifi with no VPN under a name that said nothing of the kind. Typed,
they are refused and the equivalent profile is named; read from an old state
file, they are understood and rewritten.

## Usage

    rasputin <profile>  # declare it, then converge (see the table above)
    rasputin status     # declared vs actual
    rasputin converge   # re-apply the declared profile (runs at boot)
    rasputin join       # put a venue's wifi in the supplicant config and join it
    rasputin portal     # open a bounded hole to clear a captive portal
    rasputin mac-new    # drop the pinned uplink MAC, take a fresh identity

Naming a profile writes `/etc/rasputin/mode`, then converges. Boot runs the same
converge path, so there is no drift between "what I asked for" and "what boots".

At a venue with a captive portal the order is: `join`, then `portal`, log in,
then the profile you want. Declaring a tunnel profile *before* the portal is
cleared is safe — converge probes for the portal with its own output guard
open and refuses to arm the kill switch behind one — but it cannot get you
online; only `portal` can.

## Deploy

Ansible authenticates as `admin_ssh_keys`, a passphrase-protected key, and it
cannot prompt for a passphrase itself - it needs an agent holding the key or it
fails at the connection having done nothing. On ws7 the desktop keyring loads it
already; anywhere else, `ssh-add ~/.ssh/id_ed25519` first.

### First run, or any run on a box you cannot physically reach

Stage it. Install the files without applying anything live, then bring the
firewall up behind a rollback window:

    cd ansible

    # 1. files only - rasputin, dnsmasq snippets, boot unit. Nothing applied.
    ansible-playbook site.yml --ask-become-pass -e converge_on_deploy=false

    # 2. apply mode + firewall, auto-reverting in 5 minutes
    ssh -t max@10.6.141.1 'sudo rasputin home --rollback 300'

    # 3. from a NEW terminal - prove you can still get in, then keep it
    ssh -t max@10.6.141.1 'sudo rasputin confirm'

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

RaspAP and mce888 are gone from this box, and their removal tasks are gone from
the role. A task that purges something already purged asserts nothing: it is a
no-op that still has to be read, understood and skipped on every run, and it
describes a machine that no longer exists. Git history holds the detail if a
rebuild ever needs it.

One thing they left behind is load-bearing and stays. raspapd's boot job was,
undocumented, the only thing starting `wpa_supplicant` on wlan0 — the first
reboot after masking it came up with the AP beaconing and a **dead WAN**. The
role enables `wpa_supplicant@wlan0.service` (config symlinked to
`wpa_supplicant-wlan0.conf`) as the boot-time owner of that association.

`ap_passphrase` is undefined on purpose, so `hostapd.conf` is left alone and
your AP keeps its current config and clients. Vault it to manage the AP:

    ansible-vault create group_vars/pi_router/vault.yml

### Expect during a converge

`rasputin` restarts `dhcpcd` for `eth0`, which briefly bounces the wired link —
and with it the WAN of anything downstream. Harmless, but not silent.

## Firewall

nftables, one table `inet rasputin`, regenerated on every mode switch and
validated with `nft -c` before it is ever loaded.

    input    policy drop  - established/related, loopback, some ICMP,
                            DHCP+DNS from LAN legs, SSH from admin legs
    forward  policy drop  - LAN -> egress, LAN <-> LAN, replies inbound
                            (portal mode: :80/:443 only, guests still dropped)
    output   policy drop  - ONLY when the profile requires a tunnel: wg0, LAN,
                            loopback, the WireGuard handshake, DHCP renewal
    nat      masquerade on the current egress

Nothing is accepted inbound on the uplink. On a travel router the uplink is
hostile by definition.

## Admin plane

SSH is permitted only on interfaces acting as **LAN in the current mode** —
role-based, not name-based, so it follows `eth0` when it changes job:

| uplink | admin allowed | denied |
|---|---|---|
| `wlan0` (`home`, `hotel-wifi*`) | `wlan1` AP, `eth0` | `wlan0` |
| `eth0` (`hotel-eth*`) | `wlan1` AP | `eth0` (now uplink), `wlan0` |

`admin_on_uplink: true` overrides this and exposes SSH to the venue. It defaults
to false and should stay there.

## The AP

WPA3-SAE **transition mode**: `wpa_key_mgmt=WPA-PSK SAE`. SAE clients get WPA3
and are immune to the offline dictionary attack on a captured handshake — the
actual threat at a hacker conference. WPA2-PSK stays advertised so older
clients still associate.

PMF is optional at the BSS (`ieee80211w=1`) because REQUIRED would lock out
every WPA2-only client and defeat transition mode, but `sae_require_mfp=1`
makes it mandatory for anyone who does use SAE — otherwise a WPA3 client can be
steered down to unprotected management frames and deauthed.

`sae_pwe=2` allows both hunt-and-peck and hash-to-element. H2E alone is
stronger but is refused by some older SAE clients, who are exactly the
population transition mode exists for.

`rsn_pairwise=CCMP`, not `wpa_pairwise` — with `wpa=2` there is no WPA1 element
for the latter to describe, so it was being ignored.

The passphrase is **vaulted** (`group_vars/pi_router/vault.yml`). The vault key
is `~/.rasputin-vault-pass`, outside the repo and referenced from
`ansible.cfg`. Rotating it drops every associated client until they re-join.

## avahi

Not removed — **restricted**. avahi announces this box by multicast on every
interface it may use, and no inbound firewall rule can stop it, because these
are packets we *send*; the output chain only clamps down when a tunnel is up.

`deny-interfaces` is set to whichever leg faces the venue: `wlan0` always (a
hardware invariant), plus `eth0` when a profile makes it the uplink. `.local` keeps working on
the AP; the venue hears nothing. rasputin rewrites it per mode.

### Don't lock yourself out

Tightening this remotely can strand you at a conference. Use the rollback window:

    rasputin home --rollback 300     # firewall reverts in 5 min unless confirmed
    rasputin confirm               # cancel the revert, keep the ruleset

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
  plaintext DNS path anywhere, including bootstrap — with exactly one bounded,
  timer-enforced exception, [captive portal mode](#captive-portals).
- Clients are handed **this box and nothing else** by DHCP option 6. A public
  secondary would be a standing bypass around the blocklist, the DoH upstream
  and the kill switch.
- The box resolves through blocky too — `/etc/resolv.conf` is pinned to
  `127.0.0.1`, and `DNS =` is stripped from `wg0.conf` so wg-quick cannot
  point it at Proton behind blocky's back.
- The **upstream half lives in `15-upstream.yml`, which rasputin owns**, not in
  the Ansible-written base. It is the half that changes: portal mode replaces
  the DoH endpoints with the venue's resolver, and writing the whole
  `upstreams` block from one place means there is never a question about how
  two `config.d` fragments merge. Ansible writes a DoH baseline so a
  `converge_on_deploy=false` deploy still leaves blocky an upstream at all.
- Listen addresses are **specific, never `0.0.0.0`**. The input chain already
  refuses `:53` from the uplink, but `rasputin rollback-fire` deletes that whole
  table — a wildcard bind would turn the lockout insurance into an open
  resolver on the venue's wifi. `rasputin` rewrites `20-listen.yml` per mode.

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
it in the input chain. rasputin adds `tcp dport 443` for LAN legs only when the
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

    sudo systemd-run --unit=rasputin-safety --on-active=600 /usr/local/sbin/rasputin home
    sudo systemd-run --unit=rasputin-apply --collect /usr/local/sbin/rasputin hotel-wifi
    rasputin status                      # expect: output guard : policy drop

    # instrument the drop path, then kill the tunnel
    sudo nft add rule inet rasputin output oifname \"wlan0\" counter comment \"leaktest\"
    sudo ip link del wg0
    sudo systemctl restart blocky       # force FRESH upstream connections
    sudo nft list chain inet rasputin output | grep leaktest

    sudo /usr/local/sbin/rasputin home   # converge also wipes the counter rule

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
  box — `rasputin status` reports whether it is armed.

## Captive portals

**Solved, in a bounded window.** The reason a login page was unreachable from
this box is worth stating precisely, because it was never the firewall:

- blocky's only upstream is **DoH on `:443`**, and intercepting `:443` is
  exactly what a portal does. No name resolves, so the portal's own login page
  cannot load — before a single rule of ours is consulted. No ordering trick
  escapes this: DoH *is* the thing being blocked.
- on a `vpn: true` profile, `wg-quick up` **succeeds against a network that
  drops every packet it sends**. Converge then armed the kill switch around a
  tunnel that had never handshook, and the box lost the one network it needed
  in order to log in.

`rasputin portal` opens a hole big enough to log in and no bigger, and closes it
on a timer whether or not you remember to:

    rasputin portal            # open it - 15 minutes by default
    rasputin portal --check    # probe only, change nothing
    rasputin portal --for 600  # a shorter window
    rasputin hotel-wifi        # done - back to the tunnel, keeping the MAC

While the window is open:

| | |
|---|---|
| tunnel | **down** — `wg0` cannot handshake through a portal anyway |
| clients | `tcp 80/443` out the uplink only. The venue-subnet drop stands, so other guests stay unreachable; so do the mDNS/SSDP/SMB discovery drops |
| DNS | **plaintext to the venue's resolver**, read from our own DHCP lease |
| blocklist | still on — malware and phishing stay blocked throughout |
| this box | output chain `accept`, because its resolver has to reach the venue's |

The hole is in *which protocols may leave*, never in *who may be reached*.

### The plaintext exception

This is the one place the "every query leaves over DoH" rule is broken, and it
is broken deliberately, because the alternative is a router that cannot be used
at a hotel. Three things bound it:

- it is written to `/etc/blocky/config.d/15-upstream.yml`, a file **rasputin
  rewrites on every converge** — so a converge from any cause restores DoH;
- a transient systemd timer forces exactly such a converge when the window
  expires. The marker carries its own expiry, so a reboot mid-portal **re-arms
  the timer from the recorded expiry** rather than losing the bound or
  extending it;
- `rasputin status` and the status viewer both report the upstream **in red**
  for as long as it is plaintext.

Naming a profile (`rasputin hotel-wifi`) leaves portal mode. Bare `rasputin
converge` does not — that is what boot runs, and a reboot must not silently
drop a window you are still inside.

### Detection

The probe has to work with no DNS at all, so it speaks HTTP/1.1 to a literal
anycast address and supplies its own `Host` header. `204` with no body is the
agreed "you are online" answer and a portal **cannot** give it: to show you a
login page it has to answer `200` or redirect. The redirect's `Location` *is*
the portal's login URL, and reporting it is the only way to learn that address
without a resolver.

Three outcomes, deliberately kept distinct:

| | |
|---|---|
| `204` | clear |
| `200`/`3xx` | portal — reported with its login URL |
| no TCP at all | **a dead uplink, not a portal**, and must not be treated as one |

Targets are `portal_probe_targets` (two Cloudflare anycast addresses for the
same service). Pin your own if a venue blackholes them.

### The MAC problem

A portal authorises **a MAC address**, not a person. Converge randomises the
uplink MAC every time — so the converge that *followed* a successful login used
to throw that login away, which is what made the old cloned-MAC workaround
necessary.

`rasputin portal` pins whatever MAC cleared the portal, and every converge after
it holds that MAC. Arriving somewhere new still gets a fresh identity: `rasputin
portal` drops the old pin *before* choosing the MAC the venue will authorise.
Release it deliberately when you leave:

    rasputin mac-new           # drop the pin, fresh identity, converge

`rasputin status` shows `PINNED`, or `PIN ... NOT APPLIED` if a pin exists but
the interface is not wearing it.

Randomising is per profile, because it is only worth its cost on a network that
is not yours. `home` sets `randomize_mac: false`: that hotspot is our own phone,
which recognises nobody and logs nothing, and a new MAC there only buys a new
lease — often on a new subnet — on every converge and every boot. The venue
profiles keep it on. Declining to randomise also turns off the supplicant's own
`mac_addr=1`, which would otherwise hand the network a new client at the next
association regardless; `rasputin status` reads `stable` rather than `random`
in that state.

Holding a MAC on `wlan0` took three corrections, all found on hardware and all
invisible in a dry run:

- **`wpa_supplicant`, not `ip link`, decides the address.** `mac_addr=1` in
  `wpa_supplicant.conf` randomises per ESS at association and overwrites
  whatever was set directly. Measured: rasputin set `02:c7:fc:e9:3d:56` and the
  interface came up wearing `8a:db:56:49:f7:6f`. The old comment claiming
  brcmfmac ignores `mac_addr=1` was simply wrong. rasputin now flips it to
  `mac_addr=0` while a pin is held, and back to `1` when the pin is dropped.
- **The supplicant must be stopped across the change.** Setting the MAC
  underneath a live instance lets it react to the interface bounce and
  re-randomise over the top. Stopped for the change and started after, the
  address holds. (`preassoc_mac_addr=1` still randomises during the *scan*, so
  the MAC reads wrong for a second or two before association — sampling it
  before `wpa_state=COMPLETED` reports an address about to be discarded.)
- **The instance unit owns the interface.** `wpa_supplicant@wlan0` is
  `-c …-wlan0.conf -i wlan0`; the plain `wpa_supplicant.service` on this image
  is the D-Bus daemon (`-u -s -O`) and restarting it does not touch `wlan0` at
  all. `reassociate_wifi` had been restarting the wrong one for its whole life
  — the interface bounce was doing the work by accident.

Ansible and rasputin both write `mac_addr`, so the role's `lineinfile` carries a
`regexp`. Without it, a deploy while a pin is held **appends** a second
`mac_addr=` line rather than replacing the first, and the file becomes
ambiguous about which wins. rasputin collapses duplicates on every converge.

### Converge no longer walks into the trap

On a `vpn: true` profile, converge probes for a portal **before it builds the
tunnel**, and the ordering is load-bearing in two ways that cost real debugging
to find:

- the probe runs **after** the MAC change, the reassociation and the dhcpcd
  bounce. Before that, the uplink is still on the *previous* venue's
  association and every probe comes back dead.
- the probe runs **with `wg0` down** — `wg-quick` installs a default route into
  the tunnel, so once it is up this box cannot see the venue's portal at all,
  which is exactly the state being diagnosed. `vpn_stop` is therefore
  unconditional; the tunnel is rebuilt a few lines later.

What each outcome does:

- **portal detected** — it stops, naming the login URL and the two commands to
  run. The kill switch is never armed around a tunnel that cannot reach its
  peer, and the previous ruleset is left intact.
- **no HTTP path** — a warning, then it converges fail-closed anyway. A dead
  uplink should stay dead rather than open the box.
- **clear** — the tunnel comes up, and a missing **handshake** is then reported
  as a peer or key problem rather than a portal, because a portal has already
  been ruled out. (`wg-quick up` succeeds against a network that drops
  everything, so the interface existing proves nothing on its own.)

## Status viewer

`tools/status/` is a laptop-side TUI (Rust, ratatui). Nothing is installed on
the Pi - each tick it runs one batched script over ssh (ControlMaster makes a
warm tick ~100 ms) and renders locally, so a hotel link carries bytes of text,
not full-screen repaints.

    cargo install --path tools/status
    rasputin-status                # tries 10.6.141.1, then the AP leg
    rasputin-status --once         # plain-text snapshot, for scripts

The DNS probes run from the laptop against the Pi's LAN address on purpose:
they exercise the input chain and the listener as a real client would.
`blocky healthcheck` on the box only proves the loopback listener — it passes
with the tunnel dead.

There was a third probe that queried a nonce label blocky could not have
cached, to force a live round trip through the DoH upstream. It is **gone**: on
a tethered uplink it spent roughly 285 ms of real bandwidth every tick to
re-answer a question `internet` and `external ip` already settle. The DoH
listener check is asked of the box now (`ss`) rather than probed by opening and
dropping a TCP connection to `:443`, which logged a TLS handshake error on the
Pi on every single tick.

The viewer is **quiet when nothing is wrong**. The headline states whether the
box is doing what the profile promised — `as declared`, or the faults named in
red — and rows that would only ever say "nothing to report" are not printed at
all: no portal line unless a window is open, no MAC line unless a pin is held,
no VPN section on a profile that declares no VPN.

The **INTERFACES** panel is the at-a-glance half: one row per port,
each with a small filled block at the left — a port LED on a switch faceplate.
Green for up and addressed, amber for associated but no lease yet, red for no
carrier, grey for a leg with no job in this profile (`wlan0` under
`hotel-eth`). Roles follow the profile, so the row labelled `uplink` moves
between `wlan0` and `eth0` on its own, and an idle leg reads as idle rather
than as a fault.

`wg0` is a port too. It is not physical, but it is where you look to ask "is my
traffic protected", and that belongs beside the legs it rides on. It reports
the **handshake** when up — the only proof the tunnel reaches its peer — and,
crucially, distinguishes *configured but unused by this profile* (grey,
`not used by 'home'`) from *missing where the profile requires it* (red,
`required by 'hotel-wifi'`). Calling the first "not configured" would send you
hunting for a `wg0.conf` that is present and correct.

The lamp is two blank cells carrying a background colour rather than a block
glyph, so it is a solid rectangle in any font; two cells because a terminal
cell is about half as wide as it is tall. Colour is confined to it deliberately
— a whole line of green drowns the numbers it is supposed to qualify.

Traffic accounting is deliberately cheap. Per-second rates are diffed from
`/proc/net/dev` between ticks and held in the viewer's memory; the totals are
the kernel's own since-boot counters. Nothing is installed on the Pi, nothing
is written anywhere, and **no counters are added to the ruleset the kill switch
lives in**. `--once` has no previous sample to diff against, so it prints
totals and a `—` for the rates.

It judges state against the declared profile (`vpn:` in `rasputin_profiles`).
A green ✓ marks only a promised protection verified present (wg0 up, guard
drop, on a `vpn: true` profile); a red ✗ marks a mismatch either way; an
expected-down state renders neutral ("down · no VPN in this profile").
Unknown profiles get facts with no judgement. The WAN section's gateway ping
is skipped while the output chain is `policy drop`, so the viewer never
probes around the kill switch. An open portal window and a plaintext upstream
are both rendered red for as long as they last — they are deliberate holes, so
they are stated loudly rather than quietly. Needs sudo on the Pi; it prompts, or takes
`--sudo-pass-file` for scripting.

## Two guard rails, both earned

1. **Validate before restart.** Handlers run `dnsmasq --test` first. RaspAP wrote
   an invalid fragment, dnsmasq died, and its only recovery action was `reload` —
   which cannot revive a failed unit, so it threw a PHP stack trace forever.
2. **No default route out a LAN interface.** `rasputin` asserts this after every
   converge and refuses to finish otherwise. RaspAP wrote `static routers=10.6.141.1`
   on `eth0` — a gateway pointing at the box itself — which black-holed every
   forwarded packet while the WAN was perfectly healthy.

## Known separate issues

- dhcpcd → systemd-networkd migration: deliberately deferred to the next
  rebuild (OS upgrade or fresh SD card), on the bench with keyboard access.
  The sed surgery on dhcpcd.conf is a real wart, but it is debugged and the
  whole stack - kill switch, DoH, MAC randomisation on brcmfmac - was proven
  on hardware with dhcpcd in place; migrating live re-opens all of that for
  zero functional gain. raspapd was what used to disable networkd at boot, and
  it is off the box entirely, so nothing stands in the way when the time comes.
