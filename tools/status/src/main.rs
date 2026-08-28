// rasputin-status: read-only dashboard for the travel router, run from the
// admin laptop. It does NOT install anything on the Pi - each tick it runs one
// batched shell script over ssh (ControlMaster keeps that cheap) and renders
// locally, so a hotel link carries a few hundred bytes of text per tick
// instead of full-screen TUI repaints.
//
// The DNS probes deliberately run from THIS machine against the Pi's LAN
// address: that exercises the input chain, the listener and the DoH upstream
// as a real client would. A probe on the box via loopback can pass while the
// LAN path is broken - that lesson is already paid for.

use std::collections::HashMap;
use std::io::Write as _;
use std::net::{TcpStream, ToSocketAddrs, UdpSocket};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::crossterm::terminal;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

const SECTION_MARK: &str = "-----8<----- ";
const DNS_OK_DOMAIN: &str = "example.com";
// A domain on the HaGeZi TIF list blocky imports; blocky answers 0.0.0.0.
// If the list ever drops it, the probe reports "not blocked" - swap the domain.
const DNS_BLOCKED_DOMAIN: &str = "8562.cn.com";
// Base for the cache-busting upstream probe. blocky serves a repeated name
// from cache forever, so only a never-seen label forces a round trip through
// the DoH upstream - NXDOMAIN is the healthy answer, SERVFAIL means blocky is
// up but its upstream (or the uplink) is dead.
const DNS_UPSTREAM_BASE: &str = "example.com";

// Batched collection script, run under sudo on the Pi. One round trip per tick.
const REMOTE_SCRIPT: &str = r#"
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
s apinfo;   iw dev wlan1 info 2>/dev/null
s stations; iw dev wlan1 station dump 2>/dev/null
s leases;   cat /var/lib/misc/dnsmasq.leases 2>/dev/null
s uptime;   cat /proc/uptime
s host;     hostname
"#;

struct Opts {
    host: Option<String>,
    user: String,
    identity: Option<String>,
    interval: Duration,
    once: bool,
    sudo_pass_file: Option<String>,
}

fn usage() -> ! {
    eprintln!(
        "usage: rasputin-status [--host ADDR] [--user NAME] [-i KEY] [--interval SECS]\n\
                        [--sudo-pass-file PATH] [--once]\n\
         \n\
         With no --host, tries 10.6.141.1 (eth0 leg) then 10.9.141.1 (AP leg).\n\
         With no -i, uses ~/.ssh/mce888-pi-deploy if it exists, else your ssh defaults.\n\
         --once prints one plain-text snapshot and exits (for scripts)."
    );
    std::process::exit(2);
}

fn parse_args() -> Opts {
    let mut o = Opts {
        host: None,
        user: "pi".into(),
        identity: None,
        interval: Duration::from_secs(2),
        once: false,
        sudo_pass_file: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let val = |args: &mut dyn Iterator<Item = String>| args.next().unwrap_or_else(|| usage());
        match a.as_str() {
            "--host" => o.host = Some(val(&mut args)),
            "--user" => o.user = val(&mut args),
            "-i" | "--identity" => o.identity = Some(val(&mut args)),
            "--interval" => {
                o.interval = Duration::from_secs(val(&mut args).parse().unwrap_or_else(|_| usage()))
            }
            "--sudo-pass-file" => o.sudo_pass_file = Some(val(&mut args)),
            "--once" => o.once = true,
            _ => usage(),
        }
    }
    if o.identity.is_none() {
        if let Some(home) = std::env::var_os("HOME") {
            let key = std::path::Path::new(&home).join(".ssh/mce888-pi-deploy");
            if key.exists() {
                o.identity = Some(key.to_string_lossy().into_owned());
            }
        }
    }
    o
}

fn pick_host(opts: &Opts) -> Option<String> {
    let candidates: Vec<String> = match &opts.host {
        Some(h) => vec![h.clone()],
        None => vec!["10.6.141.1".into(), "10.9.141.1".into()],
    };
    candidates.into_iter().find(|h| {
        format!("{h}:22")
            .to_socket_addrs()
            .ok()
            .and_then(|mut a| a.next())
            .and_then(|a| TcpStream::connect_timeout(&a, Duration::from_millis(1200)).ok())
            .is_some()
    })
}

fn ssh_base(opts: &Opts, host: &str) -> Command {
    let mut c = Command::new("ssh");
    c.arg("-o").arg("BatchMode=yes"); // key auth only; never hang on a prompt
    c.arg("-o").arg("ConnectTimeout=5");
    c.arg("-o").arg("ControlMaster=auto");
    c.arg("-o").arg("ControlPath=~/.ssh/rasputin-status-%C"); // %C: short hash, stays under the socket path limit
    c.arg("-o").arg("ControlPersist=120");
    if let Some(id) = &opts.identity {
        c.arg("-i").arg(id);
    }
    c.arg(format!("{}@{}", opts.user, host));
    c
}

fn ssh_run(opts: &Opts, host: &str, remote: &str, stdin_data: &str) -> Result<String, String> {
    let mut cmd = ssh_base(opts, host);
    cmd.arg(remote);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("spawn ssh: {e}"))?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin_data.as_bytes())
        .map_err(|e| format!("ssh stdin: {e}"))?;
    let out = child.wait_with_output().map_err(|e| format!("ssh: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ssh exit {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// true if the Pi allows passwordless sudo, so no password needs to be held.
fn sudo_is_passwordless(opts: &Opts, host: &str) -> bool {
    ssh_run(opts, host, "sudo -n true", "").is_ok()
}

fn prompt_password(user: &str, host: &str) -> String {
    eprint!("[sudo] password for {user}@{host}: ");
    let mut pw = String::new();
    if terminal::enable_raw_mode().is_err() {
        eprintln!("\nno terminal to prompt on - pass --sudo-pass-file");
        std::process::exit(1);
    }
    loop {
        let Ok(ev) = event::read() else {
            terminal::disable_raw_mode().ok();
            eprintln!("\nno terminal to prompt on - pass --sudo-pass-file");
            std::process::exit(1);
        };
        if let Event::Key(k) = ev {
            if k.kind != KeyEventKind::Press {
                continue;
            }
            match k.code {
                KeyCode::Enter => break,
                KeyCode::Backspace => {
                    pw.pop();
                }
                KeyCode::Char(c) => pw.push(c),
                _ => {}
            }
        }
    }
    terminal::disable_raw_mode().expect("raw mode off");
    eprintln!();
    pw
}

// ---------------------------------------------------------------------------
// Collection

#[derive(Clone, Default)]
struct DnsProbe {
    addrs: Vec<String>,
    millis: u128,
    rcode: u8,
    error: Option<String>,
}

#[derive(Clone, Default)]
struct Status {
    host: String,
    hostname: String,
    uptime_secs: u64,
    netmode: HashMap<String, String>, // "profile", "vpn (wg0)", ...
    wan_path: String,       // `ip route get 1.1.1.1`, first line
    wan_gw: String,  // "10.31.0.1 reachable 1.2" (ms) | "... unreachable" | "... unprobed" | "none"
    wan_tcp: String,   // TCP 1.1.1.1:443 from the box: "open 34" (ms) | "no-path"
    wan_extip: String, // per https://1.1.1.1/cdn-cgi/trace; "" if unreachable, "nocurl" if no curl
    ifaces: HashMap<String, String>, // name → `ip -br addr` remainder: "UP 10.6.141.1/24 ..."
    wg: String,
    ap_ssid: String,
    ap_channel: String,
    station_signals: Vec<i32>,
    leases_ap: usize,
    leases_lan: usize,
    dns_ok: DnsProbe,
    dns_blocked: DnsProbe,
    dns_upstream: DnsProbe,
    doh_listening: Option<bool>,
    error: Option<String>,
    taken_at: Option<Instant>,
}

fn parse_sections(raw: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for chunk in raw.split(SECTION_MARK).skip(1) {
        if let Some((name, body)) = chunk.split_once('\n') {
            out.insert(name.trim().to_string(), body.trim_end().to_string());
        }
    }
    out
}

fn collect(opts: &Opts, host: &str, sudo_pw: Option<&str>) -> Status {
    let mut st = Status { host: host.to_string(), ..Default::default() };

    let (remote, stdin_data) = match sudo_pw {
        None => ("sudo -n bash -s".to_string(), REMOTE_SCRIPT.to_string()),
        // sudo -S consumes the first stdin line as the password; bash -s reads the rest.
        Some(pw) => ("sudo -S -p '' bash -s".to_string(), format!("{pw}\n{REMOTE_SCRIPT}")),
    };

    match ssh_run(opts, host, &remote, &stdin_data) {
        Err(e) => st.error = Some(e),
        Ok(raw) => {
            let sections = parse_sections(&raw);
            if let Some(nm) = sections.get("netmode") {
                for line in nm.lines() {
                    if let Some((k, v)) = line.split_once(':') {
                        st.netmode.insert(k.trim().to_string(), v.trim().to_string());
                    }
                }
            }
            if let Some(wan) = sections.get("wan") {
                for line in wan.lines() {
                    let line = line.trim();
                    if let Some(rest) = line.strip_prefix("gw ") {
                        st.wan_gw = rest.to_string();
                    } else if let Some(rest) = line.strip_prefix("tcp443 ") {
                        st.wan_tcp = rest.to_string();
                    } else if let Some(rest) = line.strip_prefix("extip") {
                        st.wan_extip = rest.trim().to_string();
                    } else if !line.is_empty() && st.wan_path.is_empty() {
                        st.wan_path = line.to_string();
                    }
                }
            }
            st.wg = sections.get("wg").cloned().unwrap_or_default();
            if let Some(addrs) = sections.get("addrs") {
                for line in addrs.lines() {
                    if let Some((name, rest)) = line.trim().split_once(char::is_whitespace) {
                        st.ifaces.insert(name.to_string(), rest.trim().to_string());
                    }
                }
            }
            if let Some(info) = sections.get("apinfo") {
                for line in info.lines() {
                    let line = line.trim();
                    if let Some(ssid) = line.strip_prefix("ssid ") {
                        st.ap_ssid = ssid.to_string();
                    }
                    if line.starts_with("channel ") {
                        st.ap_channel = line.trim_start_matches("channel ").to_string();
                    }
                }
            }
            if let Some(dump) = sections.get("stations") {
                for line in dump.lines() {
                    let line = line.trim();
                    // "signal:      -50 [-50, -54] dBm" - first number is the aggregate
                    if let Some(rest) = line.strip_prefix("signal:") {
                        if let Some(n) = rest.split_whitespace().next().and_then(|t| t.parse().ok()) {
                            st.station_signals.push(n);
                        }
                    }
                }
            }
            if let Some(leases) = sections.get("leases") {
                for line in leases.lines() {
                    if let Some(ip) = line.split_whitespace().nth(2) {
                        if ip.starts_with("10.9.141.") {
                            st.leases_ap += 1;
                        } else if ip.starts_with("10.6.141.") {
                            st.leases_lan += 1;
                        }
                    }
                }
            }
            if let Some(up) = sections.get("uptime") {
                st.uptime_secs = up
                    .split_whitespace()
                    .next()
                    .and_then(|t| t.parse::<f64>().ok())
                    .unwrap_or(0.0) as u64;
            }
            st.hostname = sections.get("host").cloned().unwrap_or_default();
        }
    }

    st.dns_ok = dns_probe(host, DNS_OK_DOMAIN);
    st.dns_blocked = dns_probe(host, DNS_BLOCKED_DOMAIN);
    // Nanosecond label: unique per tick, or blocky's negative cache (a day for
    // example.com) would answer every probe after the first without going out.
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    st.dns_upstream = dns_probe(host, &format!("st{nonce}.{DNS_UPSTREAM_BASE}"));
    st.doh_listening = Some(
        format!("{host}:443")
            .to_socket_addrs()
            .ok()
            .and_then(|mut a| a.next())
            .and_then(|a| TcpStream::connect_timeout(&a, Duration::from_millis(1200)).ok())
            .is_some(),
    );
    st.taken_at = Some(Instant::now());
    st
}

// ---------------------------------------------------------------------------
// Minimal DNS client - a single hand-rolled A query, so the probe needs no
// resolver crate and times the exact wire exchange with the Pi.

fn dns_probe(server: &str, domain: &str) -> DnsProbe {
    let mut p = DnsProbe::default();
    match dns_query_a(server, domain) {
        Ok((addrs, ms, rcode)) => {
            p.addrs = addrs;
            p.millis = ms;
            p.rcode = rcode;
        }
        Err(e) => p.error = Some(e),
    }
    p
}

fn dns_query_a(server: &str, domain: &str) -> Result<(Vec<String>, u128, u8), String> {
    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    sock.set_read_timeout(Some(Duration::from_millis(1500))).unwrap();

    let id = (SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos() % 0xffff) as u16;
    let mut q = Vec::with_capacity(64);
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]); // RD, one question
    for label in domain.split('.') {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.extend_from_slice(&[0, 0, 1, 0, 1]); // root, QTYPE=A, QCLASS=IN

    let start = Instant::now();
    sock.send_to(&q, format!("{server}:53")).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 1024];
    let (n, _) = sock.recv_from(&mut buf).map_err(|_| "timeout".to_string())?;
    let ms = start.elapsed().as_millis();
    let b = &buf[..n];
    if n < 12 || b[0..2] != id.to_be_bytes() {
        return Err("bad response".into());
    }
    let rcode = b[3] & 0x0f;
    let ancount = u16::from_be_bytes([b[6], b[7]]) as usize;

    // Skip the question section.
    let mut i = 12;
    while i < n && b[i] != 0 {
        i += b[i] as usize + 1;
    }
    i += 5; // null label + qtype + qclass

    let mut addrs = Vec::new();
    for _ in 0..ancount {
        if i + 12 > n {
            break;
        }
        // Name: either a compression pointer or inline labels.
        if b[i] & 0xc0 == 0xc0 {
            i += 2;
        } else {
            while i < n && b[i] != 0 {
                i += b[i] as usize + 1;
            }
            i += 1;
        }
        let rtype = u16::from_be_bytes([b[i], b[i + 1]]);
        let rdlen = u16::from_be_bytes([b[i + 8], b[i + 9]]) as usize;
        i += 10;
        if rtype == 1 && rdlen == 4 && i + 4 <= n {
            addrs.push(format!("{}.{}.{}.{}", b[i], b[i + 1], b[i + 2], b[i + 3]));
        }
        i += rdlen;
    }
    Ok((addrs, ms, rcode))
}

// ---------------------------------------------------------------------------
// Interpretation

/// What the declared profile promises: Some(true) = tunnel + kill switch,
/// Some(false) = open egress, None = unknown profile, state facts only.
fn vpn_expected(profile: &str) -> Option<bool> {
    match profile {
        "home" | "serve" | "lan" | "uplink" | "wan" => Some(false),
        "hotel-wifi" | "hotel-eth" => Some(true),
        _ => None,
    }
}

/// Interface roles and VPN policy declared by the profile.
fn profile_desc(profile: &str) -> Option<&'static str> {
    match profile {
        "home" | "serve" | "lan" => Some("WAN wlan0 · LAN eth0 · no VPN"),
        "hotel-wifi" => Some("WAN wlan0 · LAN eth0 · VPN + kill switch"),
        "hotel-eth" => Some("WAN eth0 · VPN + kill switch"),
        "uplink" | "wan" => Some("WAN eth0 · no VPN"),
        _ => None,
    }
}

/// The physical uplink leg each profile declares. wg0 rides on this, so it is
/// derived from the profile, not from `ip route get` (which names wg0 when the
/// tunnel is up).
fn uplink_if(profile: &str) -> Option<&'static str> {
    match profile {
        "home" | "serve" | "lan" | "hotel-wifi" => Some("wlan0"),
        "hotel-eth" | "uplink" | "wan" => Some("eth0"),
        _ => None,
    }
}

/// `UP 10.6.141.1/24 fe80::1/64` → `UP · 10.6.141.1/24` (first IPv4 only)
fn fmt_ifaddr(raw: &str) -> String {
    let mut it = raw.split_whitespace();
    let state = it.next().unwrap_or("?").to_string();
    match it.find(|a| a.contains('.')) {
        Some(v4) => format!("{state} · {v4}"),
        None => format!("{state} · no address"),
    }
}

fn fmt_uptime(secs: u64) -> String {
    let (d, rem) = (secs / 86400, secs % 86400);
    let (h, m) = (rem / 3600, (rem % 3600) / 60);
    if d > 0 { format!("{d}d {h}h") } else if h > 0 { format!("{h}h {m}m") } else { format!("{m}m") }
}

/// `oifname "wlan0" masquerade` → `wlan0 (masquerade)`
fn fmt_egress(raw: &str) -> String {
    match raw.split('"').nth(1) {
        Some(ifname) => format!("{ifname} (masquerade)"),
        None => raw.to_string(),
    }
}

/// Keep the route's substance, drop the dhcp bookkeeping after " proto ".
fn fmt_route(raw: &str) -> String {
    let r = raw.trim_end_matches([';', ' ']);
    r.split(" proto ").next().unwrap_or(r).to_string()
}

/// `40 (5200 MHz), width: 80 MHz, center1: 5210 MHz` → `ch 40 · 80 MHz`
fn fmt_channel(raw: &str) -> String {
    let ch = raw.split_whitespace().next().unwrap_or("?");
    match raw.split("width: ").nth(1).and_then(|w| w.split(',').next()) {
        Some(width) => format!("ch {ch} · {}", width.trim()),
        None => format!("ch {ch}"),
    }
}

struct Judged {
    vpn: (String, bool), // text, is_mismatch
    guard: (String, bool),
    protected: bool, // tunnel profile fully enforced - the one green state
}

fn judge(st: &Status) -> Judged {
    let profile = st.netmode.get("profile").cloned().unwrap_or_default();
    let vpn_up = st.netmode.get("vpn (wg0)").map(|v| v == "UP").unwrap_or(false);
    let guard_raw = st.netmode.get("output guard").cloned().unwrap_or_else(|| "?".into());
    let guard_drop = guard_raw.contains("drop");
    let vpn_word = if vpn_up { "UP" } else { "down" };
    let guard_word: String = if guard_drop {
        "drop".into()
    } else if guard_raw.contains("accept") {
        "accept".into()
    } else {
        guard_raw.clone()
    };

    match vpn_expected(&profile) {
        Some(want) => {
            let vpn_ok = vpn_up == want;
            let guard_ok = guard_drop == want;
            Judged {
                vpn: (
                    if !vpn_ok {
                        format!("{vpn_word} ✗ expected {}", if want { "UP" } else { "down" })
                    } else if want {
                        format!("{vpn_word} ✓")
                    } else {
                        format!("{vpn_word} · no VPN in this profile")
                    },
                    !vpn_ok,
                ),
                guard: (
                    if !guard_ok {
                        format!("{guard_word} ✗ expected {}", if want { "drop" } else { "accept" })
                    } else if want {
                        format!("{guard_word} ✓")
                    } else {
                        format!("{guard_word} · no VPN in this profile")
                    },
                    !guard_ok,
                ),
                protected: want && vpn_ok && guard_ok,
            }
        }
        None => Judged {
            vpn: (vpn_word.into(), false),
            guard: (guard_word, false),
            protected: false,
        },
    }
}

fn wg_summary(st: &Status) -> Option<String> {
    if st.wg.is_empty() {
        return None;
    }
    let mut handshake = None;
    let mut transfer = None;
    for line in st.wg.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("latest handshake:") {
            handshake = Some(v.trim().to_string());
        }
        if let Some(v) = line.strip_prefix("transfer:") {
            transfer = Some(v.trim().to_string());
        }
    }
    match (handshake, transfer) {
        (Some(h), Some(t)) => Some(format!("handshake {h} · {t}")),
        (Some(h), None) => Some(format!("handshake {h}")),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Rendering. Ticks are green, mismatches and failures red, section headers
// cyan, labels dim. The profile chip is coloured by what the profile promises:
// blue = no VPN, magenta = VPN + kill switch. A ✓ marks a promised protection
// verified present; a declared absence ("down" on a no-VPN profile) is a plain
// fact, stated with its reason and no tick. --once stays uncoloured for scripts.

fn build_lines(st: &Status, plain: &Style, dim: &Style, bad: &Style, good: &Style) -> Vec<Line<'static>> {
    let j = judge(st);
    let label = |s: &str| Span::styled(format!("   {s:<12}"), *dim);
    let kv = |k: &str, v: String, style: &Style| {
        let mut spans = vec![label(k)];
        match v.strip_suffix(" ✓") {
            Some(base) => {
                spans.push(Span::styled(base.to_string(), *style));
                spans.push(Span::styled(" ✓", *good));
            }
            None => spans.push(Span::styled(v, *style)),
        }
        Line::from(spans)
    };
    let section = |s: &str| {
        Line::from(Span::styled(
            format!(" {s}"),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ))
    };
    let pick = |is_bad: bool| if is_bad { bad } else { plain };

    let profile = st.netmode.get("profile").cloned().unwrap_or_else(|| "?".into());

    let mut lines = Vec::new();

    // The profile is the headline: a reversed chip plus what it promises.
    lines.push(Line::default());
    let mut headline = vec![
        Span::raw(" "),
        Span::styled(
            format!("  {profile}  "),
            Style::default()
                .fg(match vpn_expected(&profile) {
                    Some(true) => Color::Magenta,
                    Some(false) => Color::Blue,
                    None => Color::DarkGray,
                })
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        ),
    ];
    if let Some(desc) = profile_desc(&profile) {
        headline.push(Span::styled(format!("  {desc}"), *dim));
    }
    lines.push(Line::from(headline));
    lines.push(Line::default());

    lines.push(section("WAN"));
    let upif = uplink_if(&profile);
    let uplink = match upif {
        Some(i) => {
            let a = st.ifaces.get(i).map(|r| fmt_ifaddr(r)).unwrap_or_else(|| "?".into());
            let bad = a.contains("DOWN") || a.contains("no address");
            (format!("{i} · {a}"), bad)
        }
        None => ("?".to_string(), false),
    };
    lines.push(kv("uplink", uplink.0, pick(uplink.1)));
    let gw = match st.wan_gw.as_str() {
        "" => ("?".to_string(), false),
        "none" => ("NO DEFAULT ROUTE".to_string(), true),
        g => {
            let mut it = g.split_whitespace();
            let ip = it.next().unwrap_or("?");
            let verdict = it.next().unwrap_or("?");
            match verdict {
                "reachable" => {
                    let lat = it.next().map(|r| format!(" · {r} ms")).unwrap_or_default();
                    (format!("{ip}{lat} ✓"), false)
                }
                "unprobed" => (format!("{ip} · not probed (kill switch)"), false),
                _ => (format!("{ip} UNREACHABLE"), true),
            }
        }
    };
    lines.push(kv("gateway", gw.0, pick(gw.1)));
    let inet = if st.wan_tcp.is_empty() {
        ("?".to_string(), false)
    } else if let Some(ms) = st.wan_tcp.strip_prefix("open") {
        let via = st
            .wan_path
            .split(" dev ")
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
            .unwrap_or("?");
        let lat = if ms.trim().is_empty() { String::new() } else { format!(" · {} ms", ms.trim()) };
        (format!("1.1.1.1:443 via {via}{lat} ✓"), false)
    } else {
        ("1.1.1.1:443 NO PATH".to_string(), true)
    };
    lines.push(kv("internet", inet.0, pick(inet.1)));
    let extip = match st.wan_extip.as_str() {
        "nocurl" => ("? (no curl on the box)".to_string(), false),
        // Alarming only when the path is up but the trace got no answer - the
        // captive-portal / TLS-interception signature. A dead path already
        // has its own red line.
        "" => ("no reply".to_string(), st.wan_tcp.starts_with("open")),
        ip => (ip.to_string(), false),
    };
    lines.push(kv("external ip", extip.0, pick(extip.1)));
    lines.push(Line::default());

    lines.push(section("VPN"));
    lines.push(kv("wg0", j.vpn.0.clone(), pick(j.vpn.1)));
    if let Some(wg) = wg_summary(st) {
        lines.push(kv("", wg, dim));
    }
    lines.push(kv("kill switch", j.guard.0.clone(), pick(j.guard.1)));
    if j.protected {
        lines.push(kv("", "all egress via wg0".into(), good));
    }
    lines.push(kv(
        "egress",
        st.netmode.get("egress").map(|e| fmt_egress(e)).unwrap_or_else(|| "?".into()),
        plain,
    ));
    lines.push(kv(
        "route",
        st.netmode.get("default route").map(|r| fmt_route(r)).unwrap_or_else(|| "?".into()),
        plain,
    ));
    lines.push(Line::default());

    lines.push(section("LAN"));
    let mut ap = if st.ap_ssid.is_empty() { "?".to_string() } else { st.ap_ssid.clone() };
    if !st.ap_channel.is_empty() {
        ap = format!("{ap} · {}", fmt_channel(&st.ap_channel));
    }
    if let Some(a) = st.ifaces.get("wlan1") {
        ap = format!("{ap} · {}", fmt_ifaddr(a));
    }
    lines.push(kv("wlan1 (AP)", ap, plain));
    if upif != Some("eth0") {
        let eth = st.ifaces.get("eth0").map(|r| fmt_ifaddr(r)).unwrap_or_else(|| "?".into());
        lines.push(kv("eth0", eth, plain));
    }
    let stations = if st.station_signals.is_empty() {
        "none".to_string()
    } else {
        let sigs: Vec<String> = st.station_signals.iter().map(|s| format!("{s} dBm")).collect();
        format!("{} · {}", st.station_signals.len(), sigs.join(", "))
    };
    lines.push(kv("stations", stations, plain));
    lines.push(kv("leases", format!("ap {} · lan {}", st.leases_ap, st.leases_lan), plain));
    lines.push(Line::default());

    lines.push(section("DNS"));
    let mut svc = vec![label("services")];
    for (name, key) in [("dnsmasq", "dnsmasq (dhcp)"), ("blocky", "blocky (dns)"), ("hostapd", "hostapd")] {
        let state = st.netmode.get(key).cloned().unwrap_or_else(|| "?".into());
        let ok = state == "active";
        if ok {
            svc.push(Span::styled(name.to_string(), *plain));
            svc.push(Span::styled(" ✓   ", *good));
        } else {
            svc.push(Span::styled(format!("{name} {state}   "), *bad));
        }
    }
    lines.push(Line::from(svc));

    let resolve = match (&st.dns_ok.error, st.dns_ok.addrs.first()) {
        (Some(e), _) => (format!("{DNS_OK_DOMAIN} → FAILED: {e}"), true),
        (None, Some(a)) => (format!("{DNS_OK_DOMAIN} → {a} · {} ms", st.dns_ok.millis), false),
        (None, None) => (format!("{DNS_OK_DOMAIN} → no A records"), true),
    };
    lines.push(kv("resolve", resolve.0, pick(resolve.1)));

    // The cached probe above can pass for days with the uplink dead; this one
    // cannot answer without a live round trip through the DoH upstream.
    let up = &st.dns_upstream;
    let upstream = match (&up.error, up.rcode) {
        (Some(e), _) => (format!("uncached probe → NO RESPONSE ({e})"), true),
        (None, 0) | (None, 3) => (format!("uncached probe → round trip · {} ms ✓", up.millis), false),
        (None, 2) => ("uncached probe → SERVFAIL: blocky can't reach its DoH upstream".to_string(), true),
        (None, rc) => (format!("uncached probe → rcode {rc}"), true),
    };
    lines.push(kv("upstream", upstream.0, pick(upstream.1)));

    let blk = &st.dns_blocked;
    let blocklist = match (&blk.error, blk.addrs.iter().any(|a| a == "0.0.0.0")) {
        (Some(e), _) => (format!("{DNS_BLOCKED_DOMAIN} → probe failed: {e}"), true),
        (None, true) => (format!("{DNS_BLOCKED_DOMAIN} → 0.0.0.0 ✓"), false),
        (None, false) => (format!("{DNS_BLOCKED_DOMAIN} → NOT BLOCKED"), true),
    };
    lines.push(kv("blocklist", blocklist.0, pick(blocklist.1)));

    let doh = match st.doh_listening {
        Some(true) => ("listening ✓".to_string(), false),
        Some(false) => ("NOT REACHABLE".to_string(), true),
        None => ("?".to_string(), false),
    };
    lines.push(kv("doh :443", doh.0, pick(doh.1)));

    lines
}

fn title_line(st: &Status) -> String {
    format!(
        " {} @ {} · up {} ",
        if st.hostname.is_empty() { "rasputin" } else { &st.hostname },
        st.host,
        fmt_uptime(st.uptime_secs),
    )
}

fn print_once(st: &Status) {
    // Ignore write errors: --once exists to be piped, and `| head` closing
    // the pipe early is normal, not a crash.
    let mut out = std::io::stdout().lock();
    let plain = Style::default();
    let _ = writeln!(out, "{}", title_line(st).trim_end());
    if let Some(e) = &st.error {
        let _ = writeln!(out, " collection error: {e}");
    }
    for line in build_lines(st, &plain, &plain, &plain, &plain) {
        let text: String = line.iter().map(|s| s.content.as_ref()).collect();
        let _ = writeln!(out, "{}", text.trim_end());
    }
}

fn run_tui(opts: &Opts, host: String, sudo_pw: Option<String>) {
    use ratatui::widgets::{Block, BorderType};

    let interval = opts.interval;
    let (tx, rx) = mpsc::channel::<Status>();

    // Collector thread: one batched ssh exec + local probes per tick.
    {
        let opts = Opts {
            host: opts.host.clone(),
            user: opts.user.clone(),
            identity: opts.identity.clone(),
            interval,
            once: false,
            sudo_pass_file: None,
        };
        let host = host.clone();
        std::thread::spawn(move || loop {
            let st = collect(&opts, &host, sudo_pw.as_deref());
            if tx.send(st).is_err() {
                return;
            }
            std::thread::sleep(interval);
        });
    }

    let mut terminal = ratatui::init();
    let mut latest = Status { host, ..Default::default() };

    loop {
        while let Ok(st) = rx.try_recv() {
            latest = st;
        }

        let dim = Style::default().fg(Color::DarkGray);
        let plain = Style::default();
        let bad = Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);
        let good = Style::default().fg(Color::Green);

        terminal
            .draw(|f| {
                let [body, foot] =
                    Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());

                let block = Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(dim)
                    .title(Span::styled(
                        title_line(&latest),
                        Style::default().add_modifier(Modifier::BOLD),
                    ));

                let content = if latest.taken_at.is_none() && latest.error.is_none() {
                    vec![Line::default(), Line::from(" connecting...")]
                } else {
                    build_lines(&latest, &plain, &dim, &bad, &good)
                };
                f.render_widget(Paragraph::new(content).block(block), body);

                let foot_text = match (&latest.error, latest.taken_at) {
                    (Some(e), _) => Line::from(Span::styled(format!(" {e}"), bad)),
                    (None, Some(t)) => {
                        let age = t.elapsed().as_secs();
                        if age > interval.as_secs() * 3 {
                            Line::from(Span::styled(format!(" stale: last data {age}s ago"), bad))
                        } else {
                            Line::from(Span::styled(
                                format!(" updated {age}s ago · tick {}s · q quits", interval.as_secs()),
                                dim,
                            ))
                        }
                    }
                    _ => Line::from(Span::styled(" connecting...", dim)),
                };
                f.render_widget(Paragraph::new(foot_text), foot);
            })
            .expect("draw");

        if event::poll(Duration::from_millis(150)).unwrap_or(false) {
            if let Ok(Event::Key(k)) = event::read() {
                if k.kind == KeyEventKind::Press
                    && matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
                {
                    break;
                }
            }
        }
    }
    ratatui::restore();
}

fn main() {
    let opts = parse_args();

    let Some(host) = pick_host(&opts) else {
        eprintln!("rasputin-status: no reachable host (tried eth0 and AP legs; is the Pi up?)");
        std::process::exit(1);
    };

    let sudo_pw = if let Some(path) = &opts.sudo_pass_file {
        match std::fs::read_to_string(path) {
            Ok(pw) => Some(pw.trim_end_matches('\n').to_string()),
            Err(e) => {
                eprintln!("rasputin-status: read {path}: {e}");
                std::process::exit(1);
            }
        }
    } else if sudo_is_passwordless(&opts, &host) {
        None
    } else {
        Some(prompt_password(&opts.user, &host))
    };

    if opts.once {
        let st = collect(&opts, &host, sudo_pw.as_deref());
        if let Some(e) = &st.error {
            eprintln!("rasputin-status: {e}");
        }
        print_once(&st);
        return;
    }

    run_tui(&opts, host, sudo_pw);
}
