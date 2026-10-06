//! What is actually bound right now: `lsof` and `docker ps`.
//!
//! These shell out, so they're used for `claim`/`ls` (where parity with the
//! Python script matters) and as a fallback. The refresh loop reads listening
//! sockets through libproc instead (see `darwin`).

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;
use serde::Serialize;

static LSOF_PORT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r":(\d+)$").unwrap());
static DOCKER_PORT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r":(\d+)->").unwrap());
static DOCKER_MAPPING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r":(\d+)->(\d+)").unwrap());

/// One listening socket as `lsof` reports it.
#[derive(Debug, Clone)]
pub struct LsofListener {
    pub command: String,
    pub pid: i32,
    pub port: i64,
}

pub fn lsof_listeners() -> Vec<LsofListener> {
    let Ok(out) = Command::new("lsof").args(["-nP", "-iTCP", "-sTCP:LISTEN"]).output() else {
        return Vec::new();
    };
    parse_lsof(&String::from_utf8_lossy(&out.stdout))
}

fn parse_lsof(text: &str) -> Vec<LsofListener> {
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 9 {
            continue;
        }
        if let Some(m) = LSOF_PORT.captures(f[8]) {
            if let Ok(port) = m[1].parse() {
                out.push(LsofListener {
                    command: f[0].to_string(),
                    pid: f[1].parse().unwrap_or(-1),
                    port,
                });
            }
        }
    }
    out
}

/// port -> `command(pid)` for every listening TCP socket (first one wins).
pub fn lsof_ports() -> IndexMap<i64, String> {
    let mut out = IndexMap::new();
    for l in lsof_listeners() {
        out.entry(l.port).or_insert_with(|| format!("{}({})", l.command, l.pid));
    }
    out
}

fn docker_ps(format: &str) -> Option<String> {
    let out = Command::new("docker").args(["ps", "--format", format]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// host port -> `docker:container` for running published containers.
pub fn docker_ports() -> IndexMap<i64, String> {
    let mut out = IndexMap::new();
    let Some(text) = docker_ps("{{.Names}}\t{{.Ports}}") else { return out };
    for line in text.lines() {
        let Some((name, ports)) = line.split_once('\t') else { continue };
        for m in DOCKER_PORT.captures_iter(ports) {
            if let Ok(p) = m[1].parse() {
                out.entry(p).or_insert_with(|| format!("docker:{name}"));
            }
        }
    }
    out
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Container {
    pub name: String,
    pub image: String,
    /// (host, container) pairs, deduplicated across IPv4/IPv6 bindings.
    pub ports: Vec<(u16, u16)>,
}

/// Running containers. Empty — silently — when Docker isn't running.
pub fn containers() -> Vec<Container> {
    let Some(text) = docker_ps("{{.Names}}\t{{.Image}}\t{{.Ports}}") else { return Vec::new() };
    text.lines().filter_map(parse_container_line).collect()
}

fn parse_container_line(line: &str) -> Option<Container> {
    let mut parts = line.splitn(3, '\t');
    let name = parts.next()?.to_string();
    let image = parts.next()?.to_string();
    let ports_field = parts.next().unwrap_or("");
    let mut ports: BTreeMap<(u16, u16), ()> = BTreeMap::new();
    for m in DOCKER_MAPPING.captures_iter(ports_field) {
        if let (Ok(h), Ok(c)) = (m[1].parse(), m[2].parse()) {
            ports.insert((h, c), ());
        }
    }
    Some(Container { name, image, ports: ports.into_keys().collect() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lsof() {
        let text = "COMMAND   PID USER   FD   TYPE DEVICE SIZE/OFF NODE NAME\n\
                    node    123 me   23u  IPv6 0x1      0t0  TCP *:3000 (LISTEN)\n\
                    node    123 me   24u  IPv4 0x2      0t0  TCP 127.0.0.1:3000 (LISTEN)\n\
                    rapportd 99 me   5u  IPv4 0x3      0t0  TCP [::1]:49152 (LISTEN)\n";
        let l = parse_lsof(text);
        assert_eq!(l.len(), 3);
        assert_eq!((l[0].pid, l[0].port), (123, 3000));
        assert_eq!(l[2].port, 49152);
    }

    #[test]
    fn parses_container() {
        let c = parse_container_line(
            "db\tpostgres:16\t0.0.0.0:5432->5432/tcp, [::]:5432->5432/tcp, 9000/tcp",
        )
        .unwrap();
        assert_eq!(c.ports, vec![(5432, 5432)]);
        assert_eq!(c.image, "postgres:16");
    }
}
