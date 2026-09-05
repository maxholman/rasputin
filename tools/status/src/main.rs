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

// Batched collection script, run under sudo on the Pi. One round trip per tick.
const FACTS: &str = "/usr/local/sbin/rasputin-facts";

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
         With no --host, probes 10.6.141.1 (eth0 leg) and 10.9.141.1 (AP leg) at once.\n\
         With no -i, uses your ssh defaults - ssh_config and the agent.\n\
         --once prints one plain-text snapshot and exits (for scripts)."
    );
    std::process::exit(2);
}

fn parse_args() -> Opts {
    let mut o = Opts {
        host: None,
        user: "max".into(),
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
    o
}

fn pick_host(opts: &Opts) -> Option<String> {
    let candidates: Vec<String> = match &opts.host {
        Some(h) => vec![h.clone()],
        None => vec!["10.6.141.1".into(), "10.9.141.1".into()],
    };
    // Both legs at once, first to answer wins. With eth0 unplugged the first
    // candidate is a dead address, and probing it before the second cost a
    // full connect timeout on every start. Either leg reaches the same box,
    // so there is nothing to prefer between them.
    let (tx, rx) = std::sync::mpsc::channel();
    let n = candidates.len();
    for h in candidates {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let ok = format!("{h}:22")
                .to_socket_addrs()
                .ok()
                .and_then(|mut a| a.next())
                .and_then(|a| TcpStream::connect_timeout(&a, Duration::from_millis(1200)).ok())
                .is_some();
            let _ = tx.send((h, ok));
        });
    }
    drop(tx);
    rx.iter().take(n).find(|(_, ok)| *ok).map(|(h, _)| h)
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
    ssh_run(opts, host, &format!("sudo -n -l {FACTS}"), "").is_ok()
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
    rasputin: HashMap<String, String>, // "profile", "vpn (wg0)", ...
    wan_path: String,       // `ip route get 1.1.1.1`, first line
    wan_gw: String,  // "10.31.0.1 reachable 1.2" (ms) | "... unreachable" | "... unprobed" | "none"
    wan_tcp: String,   // TCP 1.1.1.1:443 from the box: "open 34" (ms) | "no-path"
    wan_extip: String, // per https://1.1.1.1/cdn-cgi/trace; "" if unreachable, "nocurl" if no curl
    ifaces: HashMap<String, String>, // name → `ip -br addr` remainder: "UP 10.6.141.1/24 ..."
    // Since-boot byte counters straight from /proc/net/dev, and the per-second
    // rates the collector derives from consecutive samples. Totals are
    // since-boot rather than since-launch so that --once, which has no previous
    // sample to diff against, still reports something true.
    netdev: ByteCounters, // since boot
    rates: ByteCounters,  // per second; empty on the first tick, which has nothing to diff
    wlan0_link: String,                  // `iw dev wlan0 link`
    eth_speed: String,                   // Mb/s, or "-" when the link is down
    eth_carrier: bool,
    wg: String,
    ap_ssid: String,
    ap_channel: String,
    station_signals: Vec<i32>,
    leases_ap: usize,
    leases_lan: usize,
    dns_ok: DnsProbe,
    dns_blocked: DnsProbe,
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
        None => (format!("sudo -n {FACTS}"), String::new()),
        // sudo -S consumes the first stdin line as the password.
        Some(pw) => (format!("sudo -S -p '' {FACTS}"), format!("{pw}\n")),
    };

    match ssh_run(opts, host, &remote, &stdin_data) {
        Err(e) => st.error = Some(e),
        Ok(raw) => {
            let sections = parse_sections(&raw);
            if let Some(nm) = sections.get("rasputin") {
                for line in nm.lines() {
                    if let Some((k, v)) = line.split_once(':') {
                        st.rasputin.insert(k.trim().to_string(), v.trim().to_string());
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
            // "  wlan0: 1234 56 0 0 0 0 0 0  789 12 ..." - field 0 of the
            // remainder is rx bytes, field 8 is tx bytes.
            if let Some(nd) = sections.get("netdev") {
                for line in nd.lines() {
                    let Some((name, rest)) = line.split_once(':') else { continue };
                    let f: Vec<&str> = rest.split_whitespace().collect();
                    if f.len() < 9 {
                        continue;
                    }
                    if let (Ok(rx), Ok(tx)) = (f[0].parse(), f[8].parse()) {
                        st.netdev.insert(name.trim().to_string(), (rx, tx));
                    }
                }
            }
            // Asked of the box, not probed from here. Opening and dropping a TCP
            // connection to :443 every tick logged a TLS handshake error on the
            // Pi every tick, and cost a round trip to learn what `ss` already knows.
            st.doh_listening = sections.get("doh").map(|d| d.trim() != "0");
            st.wlan0_link = sections.get("wlan0").cloned().unwrap_or_default();
            if let Some(el) = sections.get("ethlink") {
                for line in el.lines() {
                    if let Some(v) = line.trim().strip_prefix("speed ") {
                        st.eth_speed = v.trim().to_string();
                    }
                    if let Some(v) = line.trim().strip_prefix("carrier ") {
                        st.eth_carrier = v.trim() == "1";
                    }
                }
            }
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

/// The tunnels the declared profile requires, as rasputin itself reports them
/// in the `vpn required` line. Empty = the profile promises open egress. None =
/// a rasputin too old to say, so nothing is judged. Read from the box rather
/// than from a table of profile names here, so that adding or reshaping a
/// profile in the role cannot leave this tool judging against a stale idea.
fn tunnels_required(st: &Status) -> Option<Vec<String>> {
    let v = st.rasputin.get("vpn required")?;
    Some(if v == "none" { vec![] } else { v.split_whitespace().map(String::from).collect() })
}

/// Some(true) = tunnel + kill switch, Some(false) = open egress, None = unknown.
fn vpn_expected(st: &Status) -> Option<bool> {
    tunnels_required(st).map(|t| !t.is_empty())
}

/// The physical uplink leg each profile declares. wg0 rides on this, so it is
/// derived from the profile, not from `ip route get` (which names wg0 when the
/// tunnel is up).
fn uplink_if(profile: &str) -> Option<&'static str> {
    match profile {
        "home" | "hotel-wifi" | "hotel-wifi-novpn" | "serve" | "lan" => Some("wlan0"),
        "hotel-eth" | "hotel-eth-novpn" | "uplink" | "wan" => Some("eth0"),
        _ => None,
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

/// `40 (5200 MHz), width: 80 MHz, center1: 5210 MHz` → `ch 40 · 80 MHz`
fn fmt_channel(raw: &str) -> String {
    let ch = raw.split_whitespace().next().unwrap_or("?");
    match raw.split("width: ").nth(1).and_then(|w| w.split(',').next()) {
        Some(width) => format!("ch {ch} · {}", width.trim()),
        None => format!("ch {ch}"),
    }
}

/// One physical port, as a switch's front panel would show it.
#[derive(Debug, PartialEq, Clone, Copy)]
enum PortState {
    Up,      // carrier and an address - the only green state
    NoAddr,  // link up, DHCP has not landed yet
    Down,    // no carrier
    Unused,  // has no role in this profile, so "down" would be a false alarm
    Unknown, // unknown profile, or the interface was not reported at all
}

/// iface → (rx bytes, tx bytes). Counters when sampled, bytes/sec once diffed.
type ByteCounters = HashMap<String, (u64, u64)>;

struct Port {
    role: &'static str,
    iface: String,
    state: PortState,
    addr: String,
    detail: String,
    rate: Option<(u64, u64)>,
    total: Option<(u64, u64)>,
}

/// 13002342 → "12.4M". At most five columns wide, so the rate fields align.
fn fmt_bytes(n: u64) -> String {
    for (unit, suffix) in [(1u64 << 30, "G"), (1 << 20, "M"), (1 << 10, "K")] {
        if n >= unit {
            let v = n as f64 / unit as f64;
            return if v < 100.0 { format!("{v:.1}{suffix}") } else { format!("{v:.0}{suffix}") };
        }
    }
    format!("{n}")
}

/// `SSID: venuewifi` + `signal: -58 dBm` → `venuewifi · -58 dBm`
fn wifi_detail(link: &str) -> String {
    let (mut ssid, mut signal) = (String::new(), String::new());
    for line in link.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("SSID:") {
            ssid = v.trim().to_string();
        }
        if let Some(v) = line.strip_prefix("signal:") {
            signal = v.trim().to_string();
        }
    }
    match (ssid.is_empty(), signal.is_empty()) {
        (false, false) => format!("{ssid} · {signal}"),
        (false, true) => ssid,
        (true, false) => signal,
        (true, true) => "not associated".into(),
    }
}

/// `wg show` for every interface, split into (name, block).
fn wg_blocks(st: &Status) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in st.wg.lines() {
        if let Some(n) = line.trim().strip_prefix("interface:") {
            out.push((n.trim().to_string(), String::new()));
        } else if let Some(last) = out.last_mut() {
            last.1.push_str(line);
            last.1.push('\n');
        }
    }
    out
}

fn wg_field(block: &str, key: &str) -> Option<String> {
    block.lines().find_map(|l| l.trim().strip_prefix(key).map(|v| v.trim().to_string()))
}

/// "latest handshake: 1 minute, 2 seconds ago" → "1 minute, 2 seconds ago"
fn wg_handshake(st: &Status, iface: &str) -> Option<String> {
    wg_blocks(st).into_iter().find(|(n, _)| n == iface).and_then(|(_, b)| wg_field(&b, "latest handshake:"))
}

fn wg_port(st: &Status, iface: &str, state: PortState, detail: String) -> Port {
    Port {
        role: "vpn",
        iface: iface.to_string(),
        state,
        addr: st
            .ifaces
            .get(iface)
            .and_then(|r| r.split_whitespace().find(|a| a.contains('.')))
            .unwrap_or("—")
            .to_string(),
        detail,
        rate: st.rates.get(iface).copied(),
        total: st.netdev.get(iface).copied(),
    }
}

fn ports(st: &Status) -> Vec<Port> {
    let profile = st.rasputin.get("profile").cloned().unwrap_or_default();
    let up_if = uplink_if(&profile);
    let mut out = Vec::new();
    for iface in ["wlan0", "wlan1", "eth0"] {
        // wlan1 is the AP by hardware invariant; the other two swap roles with
        // the profile, and whichever is not the uplink in `uplink` mode has no
        // job at all - rendering that one red would be a false alarm.
        let role = match (iface, up_if) {
            ("wlan1", _) => "ap",
            (i, Some(u)) if i == u => "uplink",
            (_, None) => "?",
            ("eth0", _) => "lan",
            _ => "—",
        };
        let raw = st.ifaces.get(iface);
        let carrier = raw.map(|r| r.split_whitespace().next() == Some("UP")).unwrap_or(false);
        let addr = raw
            .and_then(|r| r.split_whitespace().find(|a| a.contains('.')))
            .unwrap_or("—")
            .to_string();
        let state = match (raw.is_some(), role, carrier, addr.as_str()) {
            (false, _, _, _) => PortState::Unknown,
            (_, "?", _, _) => PortState::Unknown,
            (_, "—", _, _) => PortState::Unused,
            // A LAN leg with nothing plugged into it has nobody to serve. That
            // is not a fault, and painting it red for a whole hotel stay taught
            // the eye to ignore the colour.
            (_, "lan", false, _) => PortState::Unused,
            (_, _, false, _) => PortState::Down,
            (_, _, true, "—") => PortState::NoAddr,
            _ => PortState::Up,
        };
        let detail = match iface {
            "wlan0" => wifi_detail(&st.wlan0_link),
            // Channel WIDTH is dropped here on purpose: in a port list the
            // station count earns the space more than "80 MHz" does, and the
            // full channel string is still in the LAN section below.
            "wlan1" => format!(
                "ch{} · {} sta",
                st.ap_channel.split_whitespace().next().unwrap_or("?"),
                st.station_signals.len()
            ),
            _ if !st.eth_carrier => if role == "lan" { "unplugged".into() } else { "no carrier".into() },
            _ if st.eth_speed == "-" || st.eth_speed.is_empty() => "link up".into(),
            _ => format!("{} Mb/s", st.eth_speed),
        };
        out.push(Port {
            role,
            iface: iface.to_string(),
            state,
            addr,
            detail,
            rate: st.rates.get(iface).copied(),
            total: st.netdev.get(iface).copied(),
        });
    }

    // The tunnels are ports too. Not physical ones, but they are where you
    // look to ask "is my traffic protected", and that belongs beside the legs
    // they ride on. One row per tunnel the profile requires; a profile that
    // requires none gets a single grey row so the absence is stated.
    match tunnels_required(st) {
        Some(req) if !req.is_empty() => {
            for t in &req {
                let (state, detail) = if st.ifaces.contains_key(t) {
                    (
                        PortState::Up,
                        wg_handshake(st, t).map(|h| format!("handshake {h}")).unwrap_or_else(|| "no handshake yet".into()),
                    )
                } else {
                    (PortState::Down, format!("required by '{profile}'"))
                };
                out.push(wg_port(st, t, state, detail));
            }
        }
        Some(_) => {
            // Configured but deliberately unused here - NOT "not configured",
            // which would send you hunting for a wg0.conf that is present and
            // correct. Up anyway is amber: something the profile did not ask for.
            let mut present: Vec<&String> = st.ifaces.keys().filter(|k| k.starts_with("wg")).collect();
            present.sort();
            match present.first() {
                Some(w) => out.push(wg_port(st, w, PortState::NoAddr, format!("UP but '{profile}' declares no VPN"))),
                None => out.push(wg_port(st, "wg0", PortState::Unused, format!("not used by '{profile}'"))),
            }
        }
        None => {
            let up = st.ifaces.contains_key("wg0");
            out.push(wg_port(st, "wg0", PortState::Unknown, if up { "up".into() } else { "down".into() }));
        }
    }
    out
}

/// Everything that is not as the profile promised, in the order you would want
/// to hear it. `None` = the profile is not one we know, so nothing is judged;
/// `Some([])` = the box is doing what it said it would.
fn faults(st: &Status) -> Option<Vec<String>> {
    let profile = st.rasputin.get("profile").cloned().unwrap_or_default();
    let required = tunnels_required(st)?;
    let want_vpn = !required.is_empty();
    let mut f = Vec::new();

    // The uplink carries everything else, so it is named first when it is out.
    if let Some(up) = uplink_if(&profile) {
        match st.ifaces.get(up) {
            None => f.push(format!("{up} missing")),
            Some(r) if r.split_whitespace().next() != Some("UP") => f.push(format!("{up} DOWN")),
            Some(r) if !r.split_whitespace().any(|a| a.contains('.')) => {
                f.push(format!("{up} has no address"))
            }
            _ => {}
        }
    }
    if want_vpn {
        // The first tunnel carries everything; any after it carry only the
        // split domains, so losing one of those is a narrower fault.
        for (i, t) in required.iter().enumerate() {
            if !st.ifaces.contains_key(t) {
                f.push(if i == 0 {
                    format!("{t} DOWN - clients have no egress")
                } else {
                    format!("{t} DOWN - split domains have no exit")
                });
            }
        }
        if !st.rasputin.get("output guard").map(|g| g.contains("drop")).unwrap_or(false) {
            f.push("kill switch NOT ARMED".into());
        }
    }
    for (name, key) in
        [("dnsmasq", "dnsmasq (dhcp)"), ("blocky", "blocky (dns)"), ("hostapd", "hostapd")]
    {
        if st.rasputin.get(key).map(|v| v != "active").unwrap_or(false) {
            f.push(format!("{name} not running"));
        }
    }
    if st.dns_ok.error.is_some() {
        f.push("DNS not resolving".into());
    }
    // An open portal window is a deliberate hole, not a fault - but it is
    // temporary and you want to be reminded you are standing in it.
    if st.rasputin.get("portal").map(|p| p.starts_with("OPEN")).unwrap_or(false) {
        f.push("captive portal window OPEN".into());
    }
    Some(f)
}

struct Judged {
    guard: (String, bool), // text, is_mismatch
    protected: bool, // tunnel profile fully enforced - the one green state
}

fn judge(st: &Status) -> Judged {
    let guard_raw = st.rasputin.get("output guard").cloned().unwrap_or_else(|| "?".into());
    let guard_drop = guard_raw.contains("drop");
    let guard_word: String = if guard_drop {
        "drop".into()
    } else if guard_raw.contains("accept") {
        "accept".into()
    } else {
        guard_raw.clone()
    };

    match tunnels_required(st) {
        Some(req) => {
            let want = !req.is_empty();
            // Every required tunnel present, or - on a profile that wants none -
            // no tunnel present at all.
            let vpn_ok = if want {
                req.iter().all(|t| st.ifaces.contains_key(t))
            } else {
                !st.ifaces.keys().any(|k| k.starts_with("wg"))
            };
            let guard_ok = guard_drop == want;
            Judged {
                guard: (
                    if !guard_ok {
                        format!("{guard_word} ✗ expected {}", if want { "drop" } else { "accept" })
                    } else if want {
                        "armed ✓".to_string()
                    } else {
                        format!("{guard_word} · not required here")
                    },
                    !guard_ok,
                ),
                protected: want && vpn_ok && guard_ok,
            }
        }
        None => Judged { guard: (guard_word, false), protected: false },
    }
}

fn wg_summary(st: &Status) -> Vec<(String, String)> {
    wg_blocks(st)
        .into_iter()
        .filter_map(|(name, block)| {
            let h = wg_field(&block, "latest handshake:");
            let t = wg_field(&block, "transfer:");
            match (h, t) {
                (Some(h), Some(t)) => Some((name, format!("handshake {h} · {t}"))),
                (Some(h), None) => Some((name, format!("handshake {h}"))),
                _ => None,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Rendering. Ticks are green, mismatches and failures red, section headers
// cyan, labels dim. The profile chip is coloured by what the profile promises:
// blue = no VPN, magenta = VPN + kill switch. A ✓ marks a promised protection
// verified present; a declared absence ("down" on a no-VPN profile) is a plain
// fact, stated with its reason and no tick. --once stays uncoloured for scripts.

fn build_lines(
    st: &Status,
    plain: &Style,
    dim: &Style,
    bad: &Style,
    good: &Style,
    colour: bool,
) -> Vec<Line<'static>> {
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

    let profile = st.rasputin.get("profile").cloned().unwrap_or_else(|| "?".into());

    let mut lines = Vec::new();

    // The profile is the headline: a reversed chip plus what it promises.
    lines.push(Line::default());
    let mut headline = vec![
        Span::raw(" "),
        Span::styled(
            format!("  {profile}  "),
            Style::default()
                .fg(match vpn_expected(st) {
                    Some(true) => Color::Magenta,
                    Some(false) => Color::Blue,
                    None => Color::DarkGray,
                })
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        ),
    ];
    // What the topology is belongs in the port panel, which states it directly.
    // The headline's job is the one thing the panel cannot say: whether the box
    // is doing what the profile promised.
    let (verdict, verdict_style) = match faults(st) {
        None => ("unrecognised profile · facts only, nothing judged".to_string(), *dim),
        Some(f) if f.is_empty() => ("as declared".to_string(), *good),
        Some(f) => (f.join(" · "), *bad),
    };
    headline.push(Span::styled(format!("  {verdict}"), verdict_style));
    lines.push(Line::from(headline));
    lines.push(Line::default());

    // The port panel. Each row carries a small filled block - a port LED on a
    // switch faceplate - rather than a coloured row: the colour is the verdict,
    // and a whole line of it drowns the numbers it is supposed to qualify.
    // Two cells wide, because a terminal cell is about half as wide as it is
    // tall, so two of them read as a square.
    lines.push(section("INTERFACES"));
    // Header and rows share one format string so their columns cannot drift
    // apart; the three leading spaces match the row's margin plus its lamp.
    let cells = |a: &str, b: &str, c: &str, d: &str, e: &str, f: &str, g: &str, h: &str, i: &str| {
        format!(" {a:<8} {b:<6} {c:<6} {d:<18} {e:<24} {f:>7} {g:>7}   {h:>6} / {i:<6}")
    };
    lines.push(Line::from(Span::styled(
        format!("   {}", cells("role", "iface", "state", "address", "link", "↓/s", "↑/s", "total↓", "↑")),
        *dim,
    )));
    for p in ports(st) {
        let (rx, tx) = match p.rate {
            Some((rx, tx)) => (fmt_bytes(rx), fmt_bytes(tx)),
            None => ("—".into(), "—".into()),
        };
        let (trx, ttx) = match p.total {
            Some((rx, tx)) => (fmt_bytes(rx), fmt_bytes(tx)),
            None => ("—".into(), "—".into()),
        };
        // Background on blank cells, not a block glyph: a solid rectangle in
        // every font, including those with no U+2588.
        let lamp = match p.state {
            PortState::Up => Some(Color::Green),
            PortState::NoAddr => Some(Color::Yellow),
            PortState::Down => Some(Color::Red),
            PortState::Unused => Some(Color::DarkGray),
            PortState::Unknown => None,
        };
        let lamp_span = match (colour, lamp) {
            (true, Some(c)) => Span::styled("  ", Style::default().bg(c)),
            // --once is piped into scripts, so it stays uncoloured; the state
            // column already carries the same fact in words.
            _ => Span::styled("  ", *plain),
        };
        lines.push(Line::from(vec![
            Span::raw(" "),
            lamp_span,
            Span::styled(
                cells(
                    p.role,
                    &p.iface,
                    match p.state {
                        PortState::Up | PortState::NoAddr => "UP",
                        PortState::Down => "DOWN",
                        PortState::Unused => "—",
                        PortState::Unknown => "?",
                    },
                    &trunc(&p.addr, 18),
                    &trunc(&p.detail, 24),
                    &rx,
                    &tx,
                    &trx,
                    &ttx,
                ),
                *plain,
            ),
        ]));
    }
    lines.push(Line::default());

    lines.push(section("WAN"));
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
                // A gateway that drops ICMP is common at venues. It is only a
                // fault when nothing else gets out either.
                _ if st.wan_tcp.starts_with("open") => (format!("{ip} · no ping reply"), false),
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
    // "portal: closed" is an internal state name, and a row that says nothing
    // on every ordinary tick trains you to stop reading the section. Shown only
    // when a window is actually open, which is the state worth acting on.
    if let Some(portal) = st.rasputin.get("portal") {
        if !portal.starts_with("closed") {
            lines.push(kv("portal", portal.clone(), bad));
        }
    }
    // Likewise the MAC: random is the normal, uninteresting case. A pin is not -
    // it means a venue authorised this address and converge is holding it.
    if let Some(mac) = st.rasputin.get("uplink mac") {
        if !mac.ends_with("random") {
            lines.push(kv("uplink mac", mac.clone(), if mac.contains("NOT APPLIED") { bad } else { dim }));
        }
    }
    lines.push(Line::default());

    // The VPN gets a section only where it has something to say. wg0's own
    // state is a port row now, so on a no-VPN profile there is nothing left
    // here worth four lines of screen.
    if vpn_expected(st) != Some(false) {
        lines.push(section("VPN"));
        lines.push(kv("kill switch", j.guard.0.clone(), pick(j.guard.1)));
        if j.protected {
            let via = tunnels_required(st).unwrap_or_default().join(" + ");
            lines.push(kv("", format!("all egress via {via}"), good));
        }
        for (name, text) in wg_summary(st) {
            lines.push(kv(&name, text, dim));
        }
        // The split: which names take the second tunnel, and how many
        // addresses the resolver has steered so far.
        if let Some(d) = st.rasputin.get("split domains") {
            let via = tunnels_required(st).and_then(|t| t.get(1).cloned()).unwrap_or_default();
            lines.push(kv("split", format!("{d} → {via}"), dim));
        }
        if let Some(h) = st.rasputin.get("split hosts") {
            lines.push(kv("steered", h.clone(), dim));
        }
        lines.push(kv(
            "egress",
            st.rasputin.get("egress").map(|e| fmt_egress(e)).unwrap_or_else(|| "?".into()),
            plain,
        ));
        lines.push(Line::default());
    }

    lines.push(section("LAN"));
    let mut ap = if st.ap_ssid.is_empty() { "?".to_string() } else { st.ap_ssid.clone() };
    if !st.ap_channel.is_empty() {
        ap = format!("{ap} · {}", fmt_channel(&st.ap_channel));
    }
    lines.push(kv("ssid", ap, plain));
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
        let state = st.rasputin.get(key).cloned().unwrap_or_else(|| "?".into());
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
        (None, Some(a)) => (format!("{DNS_OK_DOMAIN} → {a} · {} ms ✓", st.dns_ok.millis), false),
        (None, None) => (format!("{DNS_OK_DOMAIN} → no A records"), true),
    };
    lines.push(kv("resolve", resolve.0, pick(resolve.1)));

    let blk = &st.dns_blocked;
    let blocklist = match (&blk.error, blk.addrs.iter().any(|a| a == "0.0.0.0")) {
        (Some(e), _) => (format!("{DNS_BLOCKED_DOMAIN} → probe failed: {e}"), true),
        (None, true) => (format!("{DNS_BLOCKED_DOMAIN} → 0.0.0.0 ✓"), false),
        (None, false) => (format!("{DNS_BLOCKED_DOMAIN} → NOT BLOCKED"), true),
    };
    lines.push(kv("blocklist", blocklist.0, pick(blocklist.1)));

    // A plaintext upstream means portal mode and nothing else, so it is red
    // rather than merely stated. Full URLs are noise here - the host is the
    // part you read - but plaintext is shown verbatim and in full.
    if let Some(ups) = st.rasputin.get("dns upstream") {
        let plain_dns = ups.starts_with("PLAINTEXT");
        let text = if plain_dns {
            ups.clone()
        } else {
            let hosts: Vec<&str> = ups
                .split_whitespace()
                .skip(1)
                .filter_map(|u| u.split("//").nth(1).and_then(|h| h.split('/').next()))
                .collect();
            format!("DoH · {}", hosts.join(", "))
        };
        lines.push(kv("upstream", text, pick(plain_dns)));
    }

    let doh = match st.doh_listening {
        Some(true) => ("listening ✓".to_string(), false),
        Some(false) => ("NOT REACHABLE".to_string(), true),
        None => ("?".to_string(), false),
    };
    lines.push(kv("doh :443", doh.0, pick(doh.1)));

    lines
}

fn trunc(s: &str, w: usize) -> String {
    if s.chars().count() <= w { s.to_string() } else { s.chars().take(w - 1).chain(['…']).collect() }
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
    for line in build_lines(st, &plain, &plain, &plain, &plain, false) {
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
        std::thread::spawn(move || {
            // Rates need two samples, so the previous one lives here rather
            // than in Status - nothing is written to disk and nothing is kept
            // on the Pi. The first tick therefore has no rate to report.
            let mut prev: Option<(Instant, ByteCounters)> = None;
            loop {
                let mut st = collect(&opts, &host, sudo_pw.as_deref());
                if let Some((taken, old)) = &prev {
                    let dt = taken.elapsed().as_secs_f64();
                    if dt > 0.2 {
                        for (iface, &(rx, tx_b)) in &st.netdev {
                            let Some(&(orx, otx)) = old.get(iface) else { continue };
                            // saturating: a MAC change or driver reload resets
                            // the kernel counters, and a negative delta would
                            // otherwise wrap into a nonsense gigabit spike.
                            st.rates.insert(
                                iface.clone(),
                                (
                                    (rx.saturating_sub(orx) as f64 / dt) as u64,
                                    (tx_b.saturating_sub(otx) as f64 / dt) as u64,
                                ),
                            );
                        }
                    }
                }
                prev = Some((Instant::now(), st.netdev.clone()));
                if tx.send(st).is_err() {
                    return;
                }
                std::thread::sleep(interval);
            }
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
                    build_lines(&latest, &plain, &dim, &bad, &good, true)
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

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn st(profile: &str) -> Status {
        let mut s = Status::default();
        s.rasputin.insert("profile".into(), profile.into());
        // What rasputin's `vpn required` line says for each profile; an
        // unknown profile is a rasputin that did not say.
        match profile {
            "hotel-wifi" | "hotel-eth" => {
                s.rasputin.insert("vpn required".into(), "wg1 wg0".into());
            }
            "something-new" => {}
            _ => {
                s.rasputin.insert("vpn required".into(), "none".into());
            }
        }
        s.ifaces.insert("wlan0".into(), "UP 10.31.4.88/24 fe80::1/64".into());
        s.ifaces.insert("wlan1".into(), "UP 10.9.141.1/24".into());
        s.ifaces.insert("eth0".into(), "UP 10.6.141.1/24".into());
        s.wlan0_link = "Connected to aa:bb\n\tSSID: venuewifi\n\tsignal: -58 dBm".into();
        s.eth_carrier = true;
        s.eth_speed = "1000".into();
        s.ap_channel = "36 (5180 MHz), width: 80 MHz".into();
        s.station_signals = vec![-50, -61, -44];
        s.netdev.insert("wlan0".into(), (4_509_715_660, 849_346_662));
        s.netdev.insert("wlan1".into(), (1_234_567, 89_000));
        s.netdev.insert("eth0".into(), (860_160, 94_208));
        s.netdev.insert("wg0".into(), (0, 0));
        s.rates.insert("wlan0".into(), (13_002_342, 3_250_585));
        s.rates.insert("wlan1".into(), (1_258_291, 245_760));
        s.rates.insert("eth0".into(), (860_160, 94_208));
        s
    }

    #[test]
    fn bytes_stay_narrow() {
        assert_eq!(fmt_bytes(0), "0");
        assert_eq!(fmt_bytes(999), "999");
        assert_eq!(fmt_bytes(1024), "1.0K");
        assert_eq!(fmt_bytes(860_160), "840K");
        assert_eq!(fmt_bytes(13_002_342), "12.4M");
        assert_eq!(fmt_bytes(4_509_715_660), "4.2G");
    }

    #[test]
    fn wifi_detail_survives_missing_halves() {
        assert_eq!(wifi_detail("\tSSID: foo\n\tsignal: -58 dBm"), "foo · -58 dBm");
        assert_eq!(wifi_detail("\tSSID: foo"), "foo");
        assert_eq!(wifi_detail(""), "not associated");
    }

    #[test]
    fn roles_follow_the_profile() {
        let p = ports(&st("hotel-wifi"));
        assert_eq!(p.iter().map(|p| p.role).collect::<Vec<_>>(), ["uplink", "ap", "lan", "vpn", "vpn"]);
        assert_eq!(p.iter().map(|p| p.iface.as_str()).collect::<Vec<_>>(), ["wlan0", "wlan1", "eth0", "wg1", "wg0"]);
        // eth0 takes the WAN in hotel-eth, which leaves wlan0 with no job.
        let p = ports(&st("hotel-eth"));
        assert_eq!(p.iter().map(|p| p.role).collect::<Vec<_>>(), ["—", "ap", "uplink", "vpn", "vpn"]);
        assert!(p[0].state == PortState::Unused, "an idle leg must not read as a fault");
    }

    /// wg0.conf exists and is correct on this box; `home` simply does not use
    /// it. Reporting that as "not configured" would send you hunting for a file
    /// that is already there - the distinction this test exists to hold.
    #[test]
    fn unused_vpn_is_not_the_same_as_a_broken_one() {
        let wg = |s: &Status| ports(s).pop().unwrap();

        let p = wg(&st("home"));
        assert_eq!(p.state, PortState::Unused, "grey, not red");
        assert_eq!(p.detail, "not used by \'home\'");

        // The same interface missing on a profile that promises it IS a fault.
        let p = wg(&st("hotel-wifi"));
        assert_eq!(p.state, PortState::Down);
        assert_eq!(p.detail, "required by \'hotel-wifi\'", "say who is asking for it");

        // And up where promised, it reports the handshake - the only proof the
        // tunnel actually reaches its peer.
        let mut s = st("hotel-wifi");
        s.ifaces.insert("wg0".into(), "UNKNOWN 10.2.0.2/32".into());
        s.wg = "interface: wg1\n  latest handshake: 3 seconds ago\ninterface: wg0\n  latest handshake: 12 seconds ago\n  transfer: 1.2 GiB received".into();
        let p = wg(&s);
        assert_eq!(p.state, PortState::Up);
        assert_eq!(p.detail, "handshake 12 seconds ago");
    }

    /// The headline says whether the box is doing what it promised, so silence
    /// there has to mean something.
    #[test]
    fn faults_are_named_not_implied() {
        let mut s = st("home");
        s.rasputin.insert("dnsmasq (dhcp)".into(), "active".into());
        s.rasputin.insert("blocky (dns)".into(), "active".into());
        s.rasputin.insert("hostapd".into(), "active".into());
        s.dns_ok.addrs = vec!["1.2.3.4".into()];
        assert_eq!(faults(&s), Some(vec![]), "a healthy home profile is silent");

        // hotel-wifi promises a tunnel and a kill switch; this box has neither.
        let mut h = s.clone();
        h.rasputin.insert("profile".into(), "hotel-wifi".into());
        h.rasputin.insert("vpn required".into(), "wg1 wg0".into());
        h.rasputin.insert("output guard".into(), "policy accept".into());
        let f = faults(&h).unwrap();
        assert!(f.iter().any(|x| x.contains("wg1 DOWN - clients have no egress")), "{f:?}");
        assert!(f.iter().any(|x| x.contains("wg0 DOWN - split domains have no exit")), "{f:?}");
        assert!(f.iter().any(|x| x.contains("kill switch NOT ARMED")), "{f:?}");

        // A rasputin that does not say what it requires gets no judgement.
        let mut u = s.clone();
        u.rasputin.insert("profile".into(), "something-new".into());
        u.rasputin.remove("vpn required");
        assert_eq!(faults(&u), None);
    }

    #[test]
    fn state_separates_no_carrier_from_no_lease() {
        // eth0 unplugged while it is a LAN leg has nobody to serve: grey.
        let mut s = st("home");
        s.ifaces.insert("eth0".into(), "DOWN".into());
        s.eth_carrier = false;
        assert!(ports(&s)[2].state == PortState::Unused);
        assert_eq!(ports(&s)[2].detail, "unplugged");
        // The same cable missing where eth0 IS the uplink is a fault.
        let mut s = st("hotel-eth");
        s.ifaces.insert("eth0".into(), "DOWN".into());
        s.eth_carrier = false;
        assert!(ports(&s)[2].state == PortState::Down);

        let mut s = st("home");
        s.ifaces.insert("wlan0".into(), "UP fe80::1/64".into());
        assert!(ports(&s)[0].state == PortState::NoAddr, "associated but unleased is amber, not red");

        // An unknown profile means no roles are known, so nothing is judged.
        let s = st("something-new");
        assert!(ports(&s).iter().all(|p| p.state == PortState::Unknown || p.role == "ap"));
    }

    /// The three port rows, taken by position from the INTERFACES section.
    /// Matching on interface names instead would also catch the profile
    /// headline, which reads "WAN wlan0 · LAN eth0 · no VPN".
    fn port_rows(state: &Status, colour: bool) -> Vec<Line<'static>> {
        let d = Style::default();
        let lines = build_lines(state, &d, &d, &d, &d, colour);
        let start = lines
            .iter()
            .position(|l| l.iter().map(|s| s.content.as_ref()).collect::<String>().trim() == "INTERFACES")
            .expect("panel is rendered");
        // skip the section title and the column header, then every row up to the blank line
        lines[start + 2..]
            .iter()
            .take_while(|l| !l.iter().map(|s| s.content.as_ref()).collect::<String>().trim().is_empty())
            .cloned()
            .collect()
    }

    /// The lamp is the only span that is exactly two blank cells.
    fn lamps(profile: &str, colour: bool) -> Vec<Option<Color>> {
        port_rows(&st(profile), colour)
            .iter()
            .map(|l| l.iter().find(|s| s.content == "  ").and_then(|s| s.style.bg))
            .collect()
    }

    /// The lamp carries the verdict, so its colour per state is the invariant -
    /// not the row width, now that rows are no longer painted edge to edge.
    #[test]
    fn lamp_colour_states_the_verdict() {
        assert_eq!(
            lamps("hotel-wifi", true),
            // Both tunnels are red: hotel-wifi promises two and this fixture has none.
            vec![Some(Color::Green), Some(Color::Green), Some(Color::Green), Some(Color::Red), Some(Color::Red)]
        );
        // wlan0 has no job under hotel-eth: grey, not the red of a fault.
        assert_eq!(lamps("hotel-eth", true)[0], Some(Color::DarkGray));
        // An unknown profile means the eth0/wlan0 roles are unknown, so neither
        // is judged, and nor is wg0. wlan1 still is: it is the AP by hardware
        // invariant, whatever the profile is called.
        assert_eq!(lamps("something-new", true), vec![None, Some(Color::Green), None, None]);
        // --once is piped, so nothing is coloured.
        assert!(lamps("hotel-wifi", false).iter().all(|c| c.is_none()));
    }

    /// The header and the rows are generated from one format string; this
    /// catches anyone reintroducing a hand-spaced header that drifts.
    #[test]
    fn header_columns_line_up_with_the_rows() {
        let d = Style::default();
        let lines = build_lines(&st("hotel-wifi"), &d, &d, &d, &d, false);
        let text = |l: &Line| l.iter().map(|s| s.content.as_ref()).collect::<String>();
        let start = lines.iter().position(|l| text(l).trim() == "INTERFACES").unwrap();
        let hdr = text(&lines[start + 1]);
        let row = text(&lines[start + 2]);
        for (label, field) in [("iface", "wlan0"), ("state", "UP"), ("address", "10.31.4.88/24")] {
            assert_eq!(
                hdr.find(label),
                row.find(field),
                "column {label:?} starts at a different offset to {field:?}\n  {hdr}\n  {row}"
            );
        }
    }

    #[test]
    fn lamp_goes_amber_then_red_as_a_leg_degrades() {
        let bg = |s: &Status| {
            port_rows(s, true)[0].iter().find(|x| x.content == "  ").unwrap().style.bg
        };
        let mut s = st("home");
        s.ifaces.insert("wlan0".into(), "UP fe80::1/64".into()); // associated, no lease
        assert_eq!(bg(&s), Some(Color::Yellow), "associated but unleased is amber");
        s.ifaces.insert("wlan0".into(), "DOWN".into());
        assert_eq!(bg(&s), Some(Color::Red), "no carrier is red");
    }

    /// Not an assertion - a visual check. `cargo test show_the_panel -- --nocapture`
    /// renders the whole view for a profile whose promises are NOT being met.
    #[test]
    fn show_the_panel() {
        for line in build_lines(&st("hotel-wifi"), &Style::default(), &Style::default(),
                                &Style::default(), &Style::default(), false).iter() {
            let t: String = line.iter().map(|s| s.content.as_ref()).collect();
            println!("{}", t.trim_end());
        }
    }
}
