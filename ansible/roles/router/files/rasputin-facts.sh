#!/bin/bash
# Managed by rasputin. Read-only probes for rasputin-status.
# Runs as root via a NOPASSWD rule pinned to this exact path, so it must
# stay root-owned and non-writable by the login user - see the role.
s(){ printf '\n-----8<----- %s\n' "$1"; }
s netmode;  /usr/local/sbin/netmode status 2>&1
s wan
ip -4 route get 1.1.1.1 2>&1 | head -1
gw=$(ip -4 route show default 2>/dev/null | awk '{print $3; exit}')
if [ -z "$gw" ]; then echo "gw none"
elif nft list chain inet netmode output 2>/dev/null | grep -q 'policy drop'; then echo "gw $gw unprobed"
elif rtt=$(ping -c 1 -W 2 "$gw" 2>/dev/null | awk -F'time=' '/time=/{split($2,a," ");print a[1]}'); [ -n "$rtt" ]; then echo "gw $gw reachable $rtt"
else echo "gw $gw unreachable"
fi
t0=$(date +%s%3N)
if timeout 2 bash -c 'exec 3<>/dev/tcp/1.1.1.1/443' 2>/dev/null; then echo "tcp443 open $(( $(date +%s%3N) - t0 ))"; else echo "tcp443 no-path"; fi
if command -v curl >/dev/null; then echo "extip $(curl -s --max-time 2 https://1.1.1.1/cdn-cgi/trace | awk -F= '/^ip=/{print $2}')"
else echo "extip nocurl"; fi
s wg;       wg show wg0 2>/dev/null
s addrs;    ip -br addr show 2>/dev/null
s netdev;   cat /proc/net/dev
s doh;      ss -lntp 'sport = :443' 2>/dev/null | grep -c blocky
s wlan0;    iw dev wlan0 link 2>/dev/null
s ethlink;  echo "speed $(cat /sys/class/net/eth0/speed 2>/dev/null || echo -)"
            echo "carrier $(cat /sys/class/net/eth0/carrier 2>/dev/null || echo 0)"
s apinfo;   iw dev wlan1 info 2>/dev/null
s stations; iw dev wlan1 station dump 2>/dev/null
s leases;   cat /var/lib/misc/dnsmasq.leases 2>/dev/null
s uptime;   cat /proc/uptime
s host;     hostname
