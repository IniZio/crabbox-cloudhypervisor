//! passt argument tests: no host loopback for the guest, DNS kept on its gateway.

use std::ffi::OsString;
use std::net::Ipv4Addr;
use std::path::Path;

use super::{default_gateway, passt_args};

const HEADER: &str =
    "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n";

fn table(rows: &[&str]) -> String {
    let mut out = HEADER.to_owned();
    for row in rows {
        out.push_str(row);
        out.push('\n');
    }
    out
}

fn args(routes: &str) -> Vec<String> {
    passt_args(Path::new("/run/l/passt.sock"), routes)
        .into_iter()
        .map(|a: OsString| a.into_string().unwrap())
        .collect()
}

#[test]
fn passt_never_maps_the_guest_gateway_to_host_loopback() {
    for routes in [
        table(&[]),
        table(&["eth0\t00000000\t0100A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0"]),
    ] {
        let args = args(&routes);
        let mapped = args.windows(2).any(
            |w| matches!(w, [flag, value] if flag == "--map-host-loopback" && value == "none"),
        );
        assert!(mapped, "{args:?}");
        assert!(!args.iter().any(|a| a == "--map-guest-addr"), "{args:?}");
    }
}

#[test]
fn dns_stays_on_the_gateway_the_guest_already_uses() {
    let routes = table(&[
        "docker0\t000011AC\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0",
        "enp1s0\t00000000\t0100A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0",
    ]);
    let want = [
        "--vhost-user",
        "-f",
        "--map-host-loopback",
        "none",
        "--dns-forward",
        "192.168.0.1",
        "-s",
        "/run/l/passt.sock",
    ];
    assert_eq!(args(&routes), want);
}

#[test]
fn without_a_gateway_passt_gets_no_dns_forward() {
    let routes = table(&["wg0\t00000000\t00000000\t0001\t0\t0\t0\t00000000\t0\t0\t0"]);
    assert_eq!(
        args(&routes),
        [
            "--vhost-user",
            "-f",
            "--map-host-loopback",
            "none",
            "-s",
            "/run/l/passt.sock"
        ]
    );
}

#[test]
fn the_first_default_route_with_a_gateway_wins() {
    let routes = table(&[
        "lo\t00000000\t0100007F\t0003\t0\t0\t0\t00000000\t0\t0\t0",
        "eth0\t0000000A\t0100000A\t0003\t0\t0\t0\t000000FF\t0\t0\t0",
        "eth0\t00000000\t0101000A\t0002\t0\t0\t0\t00000000\t0\t0\t0",
        "eth0\t00000000\t0100000A\t0003\t0\t0\t50\t00000000\t0\t0\t0",
        "eth1\t00000000\t0200000A\t0003\t0\t0\t10\t00000000\t0\t0\t0",
    ]);
    assert_eq!(default_gateway(&routes), Some(Ipv4Addr::new(10, 0, 0, 1)));
}

#[test]
fn a_malformed_table_has_no_gateway() {
    assert_eq!(default_gateway(""), None);
    assert_eq!(default_gateway(&table(&["eth0\tzz"])), None);
}
