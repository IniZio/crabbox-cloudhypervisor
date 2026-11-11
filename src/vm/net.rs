//! passt's arguments: the guest reaches the network and the host's resolver,
//! never the host's loopback.
//!
//! By default passt maps the guest's gateway address to the host's 127.0.0.1,
//! so any guest could reach every loopback-only service on the host.
//! `--map-host-loopback none` turns that off. The host resolver is often a
//! loopback stub (systemd-resolved on 127.0.0.53), which passt then could no
//! longer offer, so `--dns-forward <gateway>` keeps DNS, and only DNS, on the
//! gateway address. Guests restored from a boot template or checkpoint already
//! have the gateway as their nameserver, so it must stay that address.

#[cfg(test)]
mod tests;

use std::ffi::OsString;
use std::net::Ipv4Addr;
use std::path::Path;

/// The host's IPv4 routing table.
pub(super) const ROUTES: &str = "/proc/net/route";
/// Route flag: the route is up.
const RTF_UP: u32 = 0x1;
/// Route flag: the route goes through a gateway.
const RTF_GATEWAY: u32 = 0x2;

/// passt arguments serving a lease's vhost-user `socket`, given the text of
/// the host's IPv4 routing table.
#[must_use]
pub(super) fn passt_args(socket: &Path, routes: &str) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["--vhost-user", "-f", "--map-host-loopback", "none"]
        .map(OsString::from)
        .to_vec();
    if let Some(gateway) = default_gateway(routes) {
        args.push("--dns-forward".into());
        args.push(gateway.to_string().into());
    }
    args.push("-s".into());
    args.push(socket.into());
    args
}

/// The gateway of the first IPv4 default route, the one passt gives the guest.
/// None when no default route goes through a gateway, as passt then maps
/// nothing to the host either.
#[must_use]
pub(super) fn default_gateway(routes: &str) -> Option<Ipv4Addr> {
    routes.lines().skip(1).find_map(default_via)
}

/// The gateway of one routing table line, when it is a default route through one.
fn default_via(line: &str) -> Option<Ipv4Addr> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let [iface, dest, gateway, flags, _, _, _, mask, ..] = fields[..] else {
        return None;
    };
    let hex = |s: &str| u32::from_str_radix(s, 16).ok();
    let flags = hex(flags)?;
    let default = iface != "lo" && hex(dest)? == 0 && hex(mask)? == 0;
    let via = flags & RTF_UP != 0 && flags & RTF_GATEWAY != 0;
    if !(default && via) {
        return None;
    }
    // The kernel prints each address's bytes as a native-endian integer.
    Some(Ipv4Addr::from(hex(gateway)?.to_ne_bytes()))
}
