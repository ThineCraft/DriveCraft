//! Which network connection DriveCraft sends its traffic through.
//!
//! A computer with more than one connection normally uses whichever the operating
//! system prefers. DriveCraft can instead be told to send from a particular local
//! address, which makes its traffic leave through that connection and its gateway.
//! Nothing about the computer's own network settings is changed.

use std::net::IpAddr;
use std::sync::RwLock;

static BIND: RwLock<Option<IpAddr>> = RwLock::new(None);

pub fn set_bind_address(addr: Option<IpAddr>) {
    if let Ok(mut b) = BIND.write() {
        *b = addr;
    }
}

pub fn bind_address() -> Option<IpAddr> {
    BIND.read().ok().and_then(|b| *b)
}

/// One of this computer's network connections.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Link {
    /// The name the operating system shows, such as "Ethernet 3".
    pub name: String,
    pub address: IpAddr,
}

/// The connections DriveCraft could send through: those with a usable IPv4 address.
pub fn links() -> Vec<Link> {
    let mut out: Vec<Link> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|i| match i.ip() {
            IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_link_local() => Some(Link {
                name: i.name,
                address: IpAddr::V4(v4),
            }),
            _ => None,
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The address the operating system would send from if left to choose. No traffic
/// is sent: a UDP socket is only asked which local address it would use.
pub fn system_choice() -> Option<IpAddr> {
    let s = std::net::UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    s.connect(("13.107.6.152", 443)).ok()?;
    Some(s.local_addr().ok()?.ip())
}

/// What DriveCraft is sending through right now.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ActiveLink {
    /// True when the user picked it; false when the operating system chose.
    pub chosen: bool,
    pub address: Option<IpAddr>,
    pub name: Option<String>,
}

pub fn active_link() -> ActiveLink {
    let chosen = bind_address();
    let address = chosen.or_else(system_choice);
    let name = address.and_then(|a| links().into_iter().find(|l| l.address == a).map(|l| l.name));
    ActiveLink {
        chosen: chosen.is_some(),
        address,
        name,
    }
}

impl ActiveLink {
    /// For example "Ethernet 3 (192.168.1.9), chosen in DriveCraft".
    pub fn describe(&self) -> String {
        let who = if self.chosen { "chosen in DriveCraft" } else { "chosen by the system" };
        match (&self.name, &self.address) {
            (Some(n), Some(a)) => format!("{n} ({a}), {who}"),
            (None, Some(a)) => format!("{a}, {who}"),
            _ => "unknown".to_string(),
        }
    }
}

/// The starting point for every HTTP client DriveCraft creates.
pub fn client_builder() -> reqwest::ClientBuilder {
    let mut b = reqwest::Client::builder();
    if let Some(ip) = bind_address() {
        b = b.local_address(ip);
    }
    b
}
