//! Why a phone cannot find or reach dialfd, and the exact fix for this host.
//!
//! The phone needs two things from the host, and neither is dialf's to grant: an mDNS
//! responder to advertise `_dialfd._tcp` (Bonjour on macOS, avahi on Linux), and a firewall
//! that lets the phone in on the WebSocket port and on UDP 5353. When either is missing the
//! phone just says "disconnected" and `dialf devices` shows `[]` — nothing on either side
//! names the cause, so this module does, per OS, with the command or config that fixes it.
//!
//! Detection is unprivileged and conservative: a rule set we cannot read is reported as
//! "may be blocking", and one we cannot understand is not reported at all. Crying wolf about
//! a firewall that is actually open would send people off to fix the wrong thing.

use std::path::Path;
use std::process::Command;

use serde::Serialize;

pub const MDNS_PORT: u16 = 5353;

/// One thing standing between the phone and dialfd, with how to remove it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub problem: String,
    pub fix: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Distro {
    NixOs,
    Debian,
    Arch,
    Fedora,
    Suse,
    Other,
}

pub fn distro() -> Distro {
    if Path::new("/etc/NIXOS").exists() {
        return Distro::NixOs;
    }
    std::fs::read_to_string("/etc/os-release")
        .map(|t| parse_os_release(&t))
        .unwrap_or(Distro::Other)
}

/// Classify by `ID`, falling back to `ID_LIKE` (Ubuntu/Mint say `debian`, Manjaro/Endeavour
/// say `arch`, Rocky/Alma say `fedora`).
pub(crate) fn parse_os_release(text: &str) -> Distro {
    let field = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .map(|v| v.trim().trim_matches('"').to_lowercase())
            .unwrap_or_default()
    };
    let (id, like) = (field("ID"), field("ID_LIKE"));
    for word in std::iter::once(id.as_str()).chain(like.split_whitespace()) {
        match word {
            "nixos" => return Distro::NixOs,
            "debian" | "ubuntu" => return Distro::Debian,
            "arch" => return Distro::Arch,
            "fedora" | "rhel" | "centos" => return Distro::Fedora,
            "suse" | "opensuse" => return Distro::Suse,
            _ => {}
        }
    }
    Distro::Other
}

const NIXOS_AVAHI: &str = "services.avahi = { enable = true; openFirewall = true; \
     publish = { enable = true; userServices = true; }; };";

/// dialfd advertises once, at start, so every avahi fix ends with this.
const RESTART: &str = "then `dialf service restart`";

/// How to get a working `avahi-publish` (the tool *and* a running avahi-daemon it can talk to).
pub fn avahi_install_fix(d: Distro) -> String {
    let fix: String = match d {
        // publish.* matters: without it avahi-daemon refuses a non-root publisher.
        Distro::NixOs => format!("add to configuration.nix: {NIXOS_AVAHI} then `sudo nixos-rebuild switch`"),
        Distro::Debian => {
            "sudo apt install avahi-daemon avahi-utils && sudo systemctl enable --now avahi-daemon".into()
        }
        Distro::Arch => {
            "sudo pacman -S avahi && sudo systemctl enable --now avahi-daemon".into()
        }
        Distro::Fedora => {
            "sudo dnf install avahi-tools && sudo systemctl enable --now avahi-daemon".into()
        }
        Distro::Suse => {
            "sudo zypper install avahi-utils && sudo systemctl enable --now avahi-daemon".into()
        }
        Distro::Other => "install avahi (the daemon plus the `avahi-publish` tool) and start \
                          avahi-daemon"
            .into(),
    };
    format!("{fix}, {RESTART}")
}

/// Turn what `avahi-publish` said on its way out into a cause and a fix.
pub fn avahi_failure(stderr: &str, d: Distro) -> Finding {
    let mut f = avahi_cause(stderr, d);
    f.fix = format!("{}, {RESTART}", f.fix);
    f
}

fn avahi_cause(stderr: &str, d: Distro) -> Finding {
    let said = stderr.trim();
    if said.contains("Daemon not running") {
        return Finding {
            problem: "avahi-daemon is not running, so dialfd cannot advertise itself over mDNS \
                      and phones cannot discover it"
                .into(),
            fix: match d {
                Distro::NixOs => format!(
                    "add to configuration.nix: {NIXOS_AVAHI} then `sudo nixos-rebuild switch`"
                ),
                Distro::Other => "start avahi-daemon".into(),
                _ => "sudo systemctl enable --now avahi-daemon".into(),
            },
        };
    }
    if said.contains("Not permitted") || said.contains("Access denied") {
        return Finding {
            problem: "avahi-daemon refuses to publish services for non-root users, so phones \
                      cannot discover dialfd"
                .into(),
            fix: match d {
                Distro::NixOs => "set services.avahi.publish = { enable = true; userServices = \
                                  true; }; then `sudo nixos-rebuild switch`"
                    .into(),
                _ => "in /etc/avahi/avahi-daemon.conf under [publish] set \
                      disable-publishing=no and disable-user-service-publishing=no, then \
                      sudo systemctl restart avahi-daemon"
                    .into(),
            },
        };
    }
    Finding {
        problem: format!(
            "avahi-publish stopped, so phones cannot discover dialfd: {}",
            if said.is_empty() { "(it printed nothing)" } else { said }
        ),
        fix: "check `systemctl status avahi-daemon`".into(),
    }
}

// ---------------------------------------------------------------------------
// Firewalls
// ---------------------------------------------------------------------------

/// Whether a port is let in by a rule set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Open {
    Yes,
    /// Only on these interfaces (NixOS `interfaces.<if>.allowed*`, trusted interfaces) —
    /// usually a VPN like `wt0`, which is not where the phone is.
    OnlyOn(Vec<String>),
    No,
    /// Couldn't read or interpret the rules.
    Unknown,
}

/// Everything this host's firewall is known (or likely) to block for the phone.
/// Empty when there is no firewall, or when it lets both ports through.
pub fn firewall_findings(ws_port: u16) -> Vec<Finding> {
    if cfg!(target_os = "macos") {
        return macos_findings();
    }
    let d = distro();
    if unit_active("firewall.service") && d == Distro::NixOs {
        let rules = nixos_firewall_script().and_then(|p| std::fs::read_to_string(p).ok());
        let check = |proto, port| match &rules {
            Some(r) => iptables_open(r, "nixos-fw", "nixos-fw-accept", proto, port),
            None => Open::Unknown,
        };
        return nixos_findings(ws_port, check("tcp", ws_port), check("udp", MDNS_PORT));
    }
    if d == Distro::NixOs && unit_active("nftables.service") {
        // networking.nftables.enable: the rules are a generated nft file we don't parse.
        return nixos_findings(ws_port, Open::Unknown, Open::Unknown);
    }
    if ufw_enabled() {
        let rules = std::fs::read_to_string("/etc/ufw/user.rules").ok();
        let check = |proto, port| match &rules {
            Some(r) => iptables_open(r, "ufw-user-input", "ACCEPT", proto, port),
            None => Open::Unknown,
        };
        return ufw_findings(ws_port, check("tcp", ws_port), check("udp", MDNS_PORT));
    }
    if let Some(zone) = firewalld_zone() {
        return firewalld_findings(ws_port, &zone);
    }
    if unit_active("nftables.service") {
        let conf = std::fs::read_to_string("/etc/nftables.conf").ok();
        let check = |proto, port| match &conf {
            Some(c) => nft_open(c, proto, port),
            None => Open::Unknown,
        };
        return nft_findings(ws_port, check("tcp", ws_port), check("udp", MDNS_PORT));
    }
    if unit_active("iptables.service") {
        let rules = std::fs::read_to_string("/etc/iptables/iptables.rules").ok();
        let check = |proto, port| match &rules {
            Some(r) => iptables_open(r, "INPUT", "ACCEPT", proto, port),
            None => Open::Unknown,
        };
        return iptables_findings(ws_port, check("tcp", ws_port), check("udp", MDNS_PORT));
    }
    Vec::new()
}

fn unit_active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", unit])
        .status()
        .is_ok_and(|s| s.success())
}

/// What each port's state means, phrased for a person. `None` = nothing to report.
fn blocked(what: &str, proto: &str, port: u16, fw: &str, open: &Open) -> Option<String> {
    match open {
        Open::Yes => None,
        Open::No => Some(format!("the {fw} firewall blocks {proto} {port} ({what})")),
        Open::OnlyOn(ifs) => Some(format!(
            "the {fw} firewall allows {proto} {port} ({what}) only on {} — not on the \
             network the phone is on",
            ifs.join(", ")
        )),
        Open::Unknown => Some(format!(
            "the {fw} firewall is active and may block {proto} {port} ({what}); its rules \
             could not be read without root"
        )),
    }
}

const WS_WHAT: &str = "the phone's connection to dialfd";
const MDNS_WHAT: &str = "mDNS, how the phone discovers dialfd";

pub(crate) fn nixos_findings(ws_port: u16, tcp: Open, udp: Open) -> Vec<Finding> {
    let rebuild = "then `sudo nixos-rebuild switch`";
    let mut out = Vec::new();
    if let Some(problem) = blocked(WS_WHAT, "TCP", ws_port, "NixOS", &tcp) {
        out.push(Finding {
            problem,
            fix: format!(
                "add to configuration.nix: networking.firewall.allowedTCPPorts = [ {ws_port} ]; \
                 {rebuild}"
            ),
        });
    }
    if let Some(problem) = blocked(MDNS_WHAT, "UDP", MDNS_PORT, "NixOS", &udp) {
        out.push(Finding { problem, fix: avahi_install_fix(Distro::NixOs) });
    }
    out
}

pub(crate) fn ufw_findings(ws_port: u16, tcp: Open, udp: Open) -> Vec<Finding> {
    let mut out = Vec::new();
    if let Some(problem) = blocked(WS_WHAT, "TCP", ws_port, "ufw", &tcp) {
        out.push(Finding { problem, fix: format!("sudo ufw allow {ws_port}/tcp") });
    }
    if let Some(problem) = blocked(MDNS_WHAT, "UDP", MDNS_PORT, "ufw", &udp) {
        out.push(Finding { problem, fix: format!("sudo ufw allow {MDNS_PORT}/udp") });
    }
    out
}

pub(crate) fn nft_findings(ws_port: u16, tcp: Open, udp: Open) -> Vec<Finding> {
    let mut out = Vec::new();
    let fix = |rule: String| {
        format!(
            "add `{rule}` to the input chain in /etc/nftables.conf, then \
             sudo systemctl reload nftables"
        )
    };
    if let Some(problem) = blocked(WS_WHAT, "TCP", ws_port, "nftables", &tcp) {
        out.push(Finding { problem, fix: fix(format!("tcp dport {ws_port} accept")) });
    }
    if let Some(problem) = blocked(MDNS_WHAT, "UDP", MDNS_PORT, "nftables", &udp) {
        out.push(Finding { problem, fix: fix(format!("udp dport {MDNS_PORT} accept")) });
    }
    out
}

pub(crate) fn iptables_findings(ws_port: u16, tcp: Open, udp: Open) -> Vec<Finding> {
    let mut out = Vec::new();
    let fix = |proto: &str, port: u16| {
        format!(
            "add `-A INPUT -p {proto} --dport {port} -j ACCEPT` to \
             /etc/iptables/iptables.rules, then sudo systemctl restart iptables"
        )
    };
    if let Some(problem) = blocked(WS_WHAT, "TCP", ws_port, "iptables", &tcp) {
        out.push(Finding { problem, fix: fix("tcp", ws_port) });
    }
    if let Some(problem) = blocked(MDNS_WHAT, "UDP", MDNS_PORT, "iptables", &udp) {
        out.push(Finding { problem, fix: fix("udp", MDNS_PORT) });
    }
    out
}

/// The generated script behind NixOS's `firewall.service` (world-readable, in the store).
fn nixos_firewall_script() -> Option<String> {
    let out = Command::new("systemctl")
        .args(["show", "firewall.service", "-p", "ExecStart", "--value"])
        .output()
        .ok()?;
    parse_exec_start_path(&String::from_utf8_lossy(&out.stdout))
}

/// `{ path=/nix/store/…/firewall-start ; argv[]=… }` → the path.
pub(crate) fn parse_exec_start_path(s: &str) -> Option<String> {
    s.split_whitespace()
        .find_map(|t| t.strip_prefix("path="))
        .map(str::to_string)
}

/// Does an iptables rule list (iptables-save syntax, or a script of `iptables -A …` calls)
/// accept `proto`/`port` into `chain`? Understands `-p`, `--dport N[:M]`, `--dports a,b:c`
/// and `-i`, and a chain policy line (`:INPUT ACCEPT`). Rules matching on source/destination
/// or state are skipped — they don't open a port to an arbitrary phone.
pub(crate) fn iptables_open(rules: &str, chain: &str, target: &str, proto: &str, port: u16) -> Open {
    let mut only_on: Vec<String> = Vec::new();
    for line in rules.lines() {
        let t: Vec<&str> = line.split_whitespace().collect();
        if t.first().is_some_and(|f| f.strip_prefix(':') == Some(chain)) {
            if t.get(1) == Some(&"ACCEPT") {
                return Open::Yes;
            }
            continue;
        }
        let arg = |flag: &str| t.iter().position(|x| *x == flag).and_then(|i| t.get(i + 1)).copied();
        if arg("-A") != Some(chain) || arg("-j") != Some(target) {
            continue;
        }
        if ["-s", "-d", "--ctstate", "--state", "--icmp-type", "--sport"]
            .iter()
            .any(|f| t.contains(f))
        {
            continue;
        }
        let rule_proto = arg("-p");
        let dports = arg("--dport").or_else(|| arg("--dports"));
        let matches = match (rule_proto, dports) {
            // Neither protocol nor port: everything, e.g. a NixOS trusted interface.
            (None, None) => true,
            (Some(p), None) => p == proto,
            (p, Some(spec)) => p.is_none_or(|p| p == proto) && ports_cover(spec, port),
        };
        if !matches {
            continue;
        }
        match arg("-i") {
            None => return Open::Yes,
            Some("lo") => {}
            Some(iface) if !only_on.iter().any(|i| i == iface) => only_on.push(iface.to_string()),
            Some(_) => {}
        }
    }
    if only_on.is_empty() { Open::No } else { Open::OnlyOn(only_on) }
}

/// `8765`, `8000:9000`, `8000-9000`, or a comma list of those.
pub(crate) fn ports_cover(spec: &str, port: u16) -> bool {
    spec.split(',').any(|part| {
        let part = part.trim();
        match part.split_once([':', '-']) {
            Some((lo, hi)) => match (lo.parse::<u16>(), hi.parse::<u16>()) {
                (Ok(lo), Ok(hi)) => (lo..=hi).contains(&port),
                _ => false,
            },
            None => part.parse::<u16>() == Ok(port),
        }
    })
}

fn ufw_enabled() -> bool {
    let enabled = std::fs::read_to_string("/etc/ufw/ufw.conf")
        .is_ok_and(|c| c.lines().any(|l| l.trim().eq_ignore_ascii_case("ENABLED=yes")));
    // A permissive default policy means nothing is blocked whatever the rules say.
    let accepts_all = std::fs::read_to_string("/etc/default/ufw")
        .is_ok_and(|c| c.contains("DEFAULT_INPUT_POLICY=\"ACCEPT\""));
    enabled && !accepts_all
}

/// The running firewalld's default zone, or `None` when firewalld isn't running.
fn firewalld_zone() -> Option<String> {
    let run = |args: &[&str]| {
        Command::new("firewall-cmd")
            .args(args)
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    (run(&["--state"])? == "running").then(|| run(&["--get-default-zone"]))?
}

fn firewalld_findings(ws_port: u16, zone: &str) -> Vec<Finding> {
    let list = |what: &str| {
        Command::new("firewall-cmd")
            .args([&format!("--zone={zone}"), what])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    };
    firewalld_findings_from(ws_port, zone, &list("--list-ports"), &list("--list-services"))
}

/// Decide from `firewall-cmd --list-ports` (`1025-65535/tcp 8765/tcp`) and `--list-services`.
pub(crate) fn firewalld_findings_from(
    ws_port: u16,
    zone: &str,
    ports: &str,
    services: &str,
) -> Vec<Finding> {
    let has = |proto: &str, port: u16| {
        ports.split_whitespace().any(|p| {
            p.split_once('/').is_some_and(|(spec, pr)| pr == proto && ports_cover(spec, port))
        })
    };
    let open = |b: bool| if b { Open::Yes } else { Open::No };
    let tcp = open(has("tcp", ws_port));
    let udp = open(has("udp", MDNS_PORT) || services.split_whitespace().any(|s| s == "mdns"));
    let fw = format!("firewalld (zone {zone})");
    let reload = "&& sudo firewall-cmd --reload";
    let mut out = Vec::new();
    if let Some(problem) = blocked(WS_WHAT, "TCP", ws_port, &fw, &tcp) {
        out.push(Finding {
            problem,
            fix: format!("sudo firewall-cmd --permanent --add-port={ws_port}/tcp {reload}"),
        });
    }
    if let Some(problem) = blocked(MDNS_WHAT, "UDP", MDNS_PORT, &fw, &udp) {
        out.push(Finding {
            problem,
            fix: format!("sudo firewall-cmd --permanent --add-service=mdns {reload}"),
        });
    }
    out
}

/// Does an nftables config accept `proto`/`port`? Looks for `<proto> dport <spec> accept`
/// (`th dport` covers both), where spec is a port, a range, a `{ set }` or a service name
/// we know. A rule set with no input `policy drop` and no drop/reject statement blocks
/// nothing, however few ports it names.
pub(crate) fn nft_open(conf: &str, proto: &str, port: u16) -> Open {
    let named = |w: &str| match w {
        "mdns" => Some(MDNS_PORT),
        _ => w.parse::<u16>().ok(),
    };
    for line in conf.lines() {
        let line = line.split('#').next().unwrap_or("");
        if !line.contains("accept") {
            continue;
        }
        let Some(rest) = line
            .split_once(&format!("{proto} dport"))
            .or_else(|| line.split_once("th dport"))
            .map(|(_, r)| r)
        else {
            continue;
        };
        let spec = rest.split("accept").next().unwrap_or("");
        let spec = spec.replace(['{', '}'], " ");
        let hit = spec.split([',', ' ']).filter(|w| !w.is_empty()).any(|w| match w.split_once('-') {
            Some((lo, hi)) => match (named(lo), named(hi)) {
                (Some(lo), Some(hi)) => (lo..=hi).contains(&port),
                _ => false,
            },
            None => named(w) == Some(port),
        });
        if hit {
            return Open::Yes;
        }
    }
    let restrictive = conf.contains("policy drop")
        || conf.lines().any(|l| {
            let l = l.split('#').next().unwrap_or("").trim();
            l == "drop" || l == "reject" || l.ends_with(" drop") || l.contains(" reject")
        });
    if restrictive { Open::No } else { Open::Yes }
}

// ---------------------------------------------------------------------------
// macOS application firewall
// ---------------------------------------------------------------------------

const SOCKETFILTERFW: &str = "/usr/libexec/ApplicationFirewall/socketfilterfw";

fn macos_findings() -> Vec<Finding> {
    let run = |args: &[&str]| {
        Command::new(SOCKETFILTERFW)
            .args(args)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    };
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "dialf".into());
    macos_findings_from(
        &exe,
        &run(&["--getglobalstate"]),
        &run(&["--getblockall"]),
        &run(&["--getappblocked", &exe]),
    )
}

/// Interpret socketfilterfw's answers. Bonjour itself (mDNSResponder) is an Apple service the
/// application firewall always lets through, so only the WebSocket listener is at risk — and
/// it is keyed to the *versioned* binary path, so, like the microphone grant, every upgrade
/// needs it again.
pub(crate) fn macos_findings_from(exe: &str, global: &str, block_all: &str, app: &str) -> Vec<Finding> {
    let on = |s: &str| s.contains("enabled") || s.contains("State = 1") || s.contains("State = 2");
    if !on(global) {
        return Vec::new();
    }
    let allow = format!(
        "sudo {SOCKETFILTERFW} --add '{exe}' && sudo {SOCKETFILTERFW} --unblockapp '{exe}' \
         (repeat after every dialf upgrade: the path is versioned)"
    );
    if block_all.contains("enabled") && !block_all.contains("disabled") {
        return vec![Finding {
            problem: "the macOS firewall is set to block ALL incoming connections, so the \
                      phone cannot connect to dialfd"
                .into(),
            fix: format!("sudo {SOCKETFILTERFW} --setblockall off, then {allow}"),
        }];
    }
    if app.contains("permitted") {
        return Vec::new();
    }
    let problem = if app.contains("blocked") {
        "the macOS firewall blocks incoming connections to dialf, so the phone cannot connect"
    } else {
        "the macOS firewall is on and dialf is not in its allow list; incoming phone \
         connections may be refused"
    };
    vec![Finding { problem: problem.into(), fix: allow }]
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

/// One fix and every problem it solves.
#[derive(Debug, PartialEq, Eq)]
pub struct Step {
    pub problems: Vec<String>,
    pub fix: String,
}

/// Group findings by fix, so a single step isn't printed twice (on NixOS the avahi block both
/// installs the responder and opens UDP 5353). Order of first appearance is kept.
pub fn group_by_fix(findings: Vec<Finding>) -> Vec<Step> {
    let mut out: Vec<Step> = Vec::new();
    for f in findings {
        match out.iter_mut().find(|s| s.fix == f.fix) {
            Some(s) => s.problems.push(f.problem),
            None => out.push(Step { problems: vec![f.problem], fix: f.fix }),
        }
    }
    out
}

/// The report `dialf devices` prints when no phone is connected. "No problems detected"
/// rather than "ready": an unrecognised firewall or the router can still be in the way.
pub fn render_no_phones(findings: Vec<Finding>, checklist: &[String]) -> String {
    let mut s = String::from("No phones connected.\n");
    let steps = group_by_fix(findings);
    if steps.is_empty() {
        s.push_str("No problems detected on this host.\n");
    } else {
        s.push_str("This host is keeping them out:\n");
        for step in &steps {
            for p in &step.problems {
                s.push_str(&format!("  ✗ {p}\n"));
            }
            s.push_str(&format!("    fix: {}\n", step.fix));
        }
    }
    if !checklist.is_empty() {
        s.push_str("Also check:\n");
        for line in checklist {
            s.push_str(&format!("  - {line}\n"));
        }
    }
    s
}

/// The hints that don't depend on detection: things only the person holding the phone can
/// check, and the manual-address fallback that sidesteps mDNS entirely.
pub fn checklist(ws_port: u16) -> Vec<String> {
    let mut addrs: Vec<String> = crate::share::local_addresses()
        .into_iter()
        // Overlay (100.64/10) and Docker bridge addresses are not where a WiFi phone is.
        .filter(|ip| !(ip.octets()[0] == 100 && (64..128).contains(&ip.octets()[1])))
        .filter(|ip| !(ip.octets()[0] == 172 && ip.octets()[1] == 17))
        .map(|ip| format!("{ip}:{ws_port}"))
        .collect();
    if addrs.is_empty() {
        addrs.push(format!("<this machine's LAN IP>:{ws_port}"));
    }
    vec![
        "the phone must be on the same subnet: a guest network or a router with \
         client/AP isolation blocks phone-to-computer traffic (2.4 GHz vs 5 GHz on the same \
         router is fine)"
            .into(),
        format!(
            "to skip discovery, type this machine's address into the app's \"dialfd address\" \
             field: {}",
            addrs.join(" or ")
        ),
        "if the phone reaches dialfd but is refused, the shared keys differ — look for \
         close code 4001 in the dialfd log"
            .into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_release_is_classified_by_id_then_id_like() {
        assert_eq!(parse_os_release("ID=nixos\n"), Distro::NixOs);
        assert_eq!(parse_os_release("ID=ubuntu\nID_LIKE=debian\n"), Distro::Debian);
        assert_eq!(parse_os_release("ID=linuxmint\nID_LIKE=\"ubuntu debian\"\n"), Distro::Debian);
        assert_eq!(parse_os_release("ID=arch\n"), Distro::Arch);
        assert_eq!(parse_os_release("ID=endeavouros\nID_LIKE=arch\n"), Distro::Arch);
        assert_eq!(parse_os_release("ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\n"), Distro::Fedora);
        assert_eq!(parse_os_release("ID=\"opensuse-tumbleweed\"\nID_LIKE=\"opensuse suse\"\n"), Distro::Suse);
        assert_eq!(parse_os_release("ID=alpine\n"), Distro::Other);
        // VERSION_ID must not be mistaken for ID.
        assert_eq!(parse_os_release("VERSION_ID=arch\nID=alpine\n"), Distro::Other);
    }

    #[test]
    fn avahi_fixes_name_each_distros_packages() {
        assert!(avahi_install_fix(Distro::Debian).contains("apt install avahi-daemon avahi-utils"));
        assert!(avahi_install_fix(Distro::Arch).contains("pacman -S avahi"));
        assert!(avahi_install_fix(Distro::Fedora).contains("avahi-tools"));
        let nix = avahi_install_fix(Distro::NixOs);
        assert!(nix.contains("userServices = true") && nix.contains("nixos-rebuild"), "{nix}");
        assert!(!nix.contains("apt"), "{nix}");
    }

    #[test]
    fn avahi_errors_map_to_their_cause() {
        let down = avahi_failure("Failed to create client object: Daemon not running\n", Distro::Arch);
        assert!(down.problem.contains("not running"), "{down:?}");
        assert!(down.fix.contains("systemctl enable --now avahi-daemon"), "{down:?}");

        let denied = avahi_failure("Failed to add service: Not permitted\n", Distro::Debian);
        assert!(denied.fix.contains("disable-user-service-publishing=no"), "{denied:?}");
        let denied_nix = avahi_failure("Failed to add service: Not permitted\n", Distro::NixOs);
        assert!(denied_nix.fix.contains("userServices"), "{denied_nix:?}");

        let other = avahi_failure("something new\n", Distro::Other);
        assert!(other.problem.contains("something new"), "{other:?}");
    }

    /// Lines as they appear in NixOS's generated firewall-start script.
    const NIXOS_FW: &str = "\
ip46tables -A nixos-fw -i lo -j nixos-fw-accept
ip46tables -A nixos-fw -m conntrack --ctstate ESTABLISHED,RELATED -j nixos-fw-accept
ip46tables -A nixos-fw -p tcp --dport 22 -j nixos-fw-accept
ip46tables -A nixos-fw -p udp --dport 5353 -j nixos-fw-accept -i wt0
ip46tables -A nixos-fw -p udp --dport 60000:61000 -j nixos-fw-accept
ip6tables -A nixos-fw -d fe80::/64 -p udp --dport 546 -j nixos-fw-accept
";

    #[test]
    fn nixos_rules_are_read_per_port_and_interface() {
        let chk = |proto, port| iptables_open(NIXOS_FW, "nixos-fw", "nixos-fw-accept", proto, port);
        assert_eq!(chk("tcp", 22), Open::Yes);
        // This exact machine's state: mDNS open only on the VPN, dialfd's port shut.
        assert_eq!(chk("udp", 5353), Open::OnlyOn(vec!["wt0".into()]));
        assert_eq!(chk("tcp", 8765), Open::No);
        assert_eq!(chk("udp", 60500), Open::Yes);
        // A port rule for tcp does not open udp.
        assert_eq!(chk("udp", 22), Open::No);
        // Link-local-destination rule is not a general opening.
        assert_eq!(chk("udp", 546), Open::No);

        let open = format!("{NIXOS_FW}ip46tables -A nixos-fw -p tcp --dport 8765 -j nixos-fw-accept\n");
        assert_eq!(iptables_open(&open, "nixos-fw", "nixos-fw-accept", "tcp", 8765), Open::Yes);
        // A trusted interface opens everything, but only there.
        let trusted = "ip46tables -A nixos-fw -i wt0 -j nixos-fw-accept\n";
        assert_eq!(
            iptables_open(trusted, "nixos-fw", "nixos-fw-accept", "tcp", 8765),
            Open::OnlyOn(vec!["wt0".into()])
        );
    }

    #[test]
    fn nixos_findings_say_what_to_put_in_configuration_nix() {
        let f = nixos_findings(8765, Open::No, Open::OnlyOn(vec!["wt0".into()]));
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(f[0].problem.contains("TCP 8765"), "{f:?}");
        assert!(f[0].fix.contains("allowedTCPPorts = [ 8765 ]"), "{f:?}");
        assert!(f[1].problem.contains("only on wt0"), "{f:?}");
        assert!(f[1].fix.contains("openFirewall = true"), "{f:?}");
        assert!(nixos_findings(8765, Open::Yes, Open::Yes).is_empty());
    }

    #[test]
    fn exec_start_path_is_extracted() {
        let s = "{ path=/nix/store/abc-firewall-start/bin/firewall-start ; argv[]=firewall-start ; ignore_errors=no }";
        assert_eq!(
            parse_exec_start_path(s).as_deref(),
            Some("/nix/store/abc-firewall-start/bin/firewall-start")
        );
        assert_eq!(parse_exec_start_path(""), None);
    }

    #[test]
    fn ufw_user_rules_are_read() {
        let rules = "\
*filter
:ufw-user-input - [0:0]
-A ufw-user-input -p tcp --dport 22 -j ACCEPT
-A ufw-user-input -p udp --dport 5353 -j ACCEPT
-A ufw-user-input -p tcp -m multiport --dports 8000:9000,443 -j ACCEPT
COMMIT
";
        let chk = |proto, port| iptables_open(rules, "ufw-user-input", "ACCEPT", proto, port);
        assert_eq!(chk("udp", 5353), Open::Yes);
        assert_eq!(chk("tcp", 8765), Open::Yes, "multiport range");
        assert_eq!(chk("tcp", 7000), Open::No);
        let f = ufw_findings(7000, Open::No, Open::Unknown);
        assert_eq!(f[0].fix, "sudo ufw allow 7000/tcp");
        assert!(f[1].problem.contains("could not be read"), "{f:?}");
        assert_eq!(f[1].fix, "sudo ufw allow 5353/udp");
    }

    #[test]
    fn plain_iptables_policy_and_rules() {
        let rules = ":INPUT DROP [0:0]\n-A INPUT -p tcp --dport 22 -j ACCEPT\n";
        assert_eq!(iptables_open(rules, "INPUT", "ACCEPT", "tcp", 8765), Open::No);
        assert_eq!(iptables_open(":INPUT ACCEPT [0:0]\n", "INPUT", "ACCEPT", "tcp", 8765), Open::Yes);
    }

    #[test]
    fn firewalld_reads_ranges_and_the_mdns_service() {
        // Fedora Workstation's default zone opens every high port and mdns.
        let fedora = firewalld_findings_from(
            8765,
            "FedoraWorkstation",
            "1025-65535/udp 1025-65535/tcp\n",
            "dhcpv6-client mdns samba-client ssh\n",
        );
        assert!(fedora.is_empty(), "{fedora:?}");

        let public = firewalld_findings_from(8765, "public", "", "ssh dhcpv6-client\n");
        assert_eq!(public.len(), 2, "{public:?}");
        assert!(public[0].fix.contains("--add-port=8765/tcp"), "{public:?}");
        assert!(public[1].fix.contains("--add-service=mdns"), "{public:?}");
    }

    #[test]
    fn nftables_config_is_read() {
        // Arch's shipped /etc/nftables.conf, abridged.
        let arch = "\
table inet filter {
  chain input {
    type filter hook input priority filter
    policy drop
    ct state invalid drop comment \"early drop of invalid connections\"
    ct state {established, related} accept
    iif lo accept
    tcp dport ssh accept
    pkttype host limit rate 5/second counter reject with icmpx type admin-prohibited
    counter
  }
}
";
        assert_eq!(nft_open(arch, "tcp", 8765), Open::No);
        assert_eq!(nft_open(arch, "udp", 5353), Open::No);
        let opened = arch.replace(
            "tcp dport ssh accept",
            "tcp dport { ssh, 8000-9000 } accept\n    udp dport mdns accept",
        );
        assert_eq!(nft_open(&opened, "tcp", 8765), Open::Yes);
        assert_eq!(nft_open(&opened, "udp", 5353), Open::Yes);
        // Nothing dropped: nothing blocked.
        assert_eq!(nft_open("table inet filter { chain input { type filter hook input priority 0; } }", "tcp", 8765), Open::Yes);
        assert!(nft_findings(8765, Open::No, Open::Yes)[0].fix.contains("tcp dport 8765 accept"));
    }

    #[test]
    fn one_fix_is_listed_once() {
        let missing = Finding {
            problem: "avahi-publish is not installed".into(),
            fix: avahi_install_fix(Distro::NixOs),
        };
        let firewall = nixos_findings(8765, Open::No, Open::No);
        let steps = group_by_fix([vec![missing], firewall].concat());
        // avahi+5353 share the NixOS avahi block; the TCP port is its own step.
        assert_eq!(steps.len(), 2, "{steps:?}");
        assert_eq!(steps[0].problems.len(), 2, "{steps:?}");
        assert!(steps[0].problems[0].contains("avahi-publish"), "{steps:?}");
        assert!(steps[0].problems[1].contains("UDP 5353"), "{steps:?}");
        assert!(steps[1].problems[0].contains("TCP 8765"), "{steps:?}");
    }

    #[test]
    fn the_report_lists_each_cause_once_under_its_fix() {
        let f = |p: &str, fix: &str| Finding { problem: p.into(), fix: fix.into() };
        let out = render_no_phones(
            vec![f("no avahi", "enable avahi"), f("5353 shut", "enable avahi"), f("8765 shut", "open 8765")],
            &["same subnet".into()],
        );
        assert_eq!(
            out,
            "No phones connected.\n\
             This host is keeping them out:\n  \
               ✗ no avahi\n  \
               ✗ 5353 shut\n    \
                 fix: enable avahi\n  \
               ✗ 8765 shut\n    \
                 fix: open 8765\n\
             Also check:\n  \
               - same subnet\n"
        );
        assert_eq!(out.matches("No phones connected").count(), 1);
    }

    #[test]
    fn a_clean_host_claims_only_what_was_checked() {
        let out = render_no_phones(Vec::new(), &["same subnet".into()]);
        assert!(out.starts_with("No phones connected.\nNo problems detected on this host.\n"), "{out}");
        assert!(!out.contains("ready"), "{out}");
        // No checklist (loopback bind): no dangling heading.
        assert!(!render_no_phones(Vec::new(), &[]).contains("Also check"));
    }

    #[test]
    fn port_specs() {
        assert!(ports_cover("8765", 8765));
        assert!(ports_cover("8000:9000", 8765));
        assert!(ports_cover("8000-9000", 8765));
        assert!(ports_cover("22,443,8765", 8765));
        assert!(!ports_cover("22,443", 8765));
        assert!(!ports_cover("ssh", 22));
    }

    #[test]
    fn macos_application_firewall() {
        let exe = "/x/vendor/dialf-0.3.12-darwin-arm64/dialf";
        let off = "Firewall is disabled. (State = 0)\n";
        let on = "Firewall is enabled. (State = 1)\n";
        let no_block_all = "Firewall has block all state set to disabled.\n";
        assert!(macos_findings_from(exe, off, "", "").is_empty());
        assert!(macos_findings_from(
            exe, on, no_block_all,
            &format!("The application {exe} is permitted to receive incoming connections\n")
        )
        .is_empty());

        let blocked = macos_findings_from(
            exe, on, no_block_all,
            &format!("The application {exe} is blocked from receiving incoming connections\n"),
        );
        assert!(blocked[0].problem.contains("blocks incoming"), "{blocked:?}");
        assert!(blocked[0].fix.contains("--unblockapp '/x/vendor/dialf-0.3.12"), "{blocked:?}");
        assert!(blocked[0].fix.contains("every dialf upgrade"), "{blocked:?}");

        let unlisted = macos_findings_from(exe, on, no_block_all, "The application is not part of the firewall\n");
        assert!(unlisted[0].problem.contains("may be refused"), "{unlisted:?}");

        let all = macos_findings_from(exe, on, "Firewall has block all state set to enabled.\n", "");
        assert!(all[0].fix.contains("--setblockall off"), "{all:?}");
    }
}
