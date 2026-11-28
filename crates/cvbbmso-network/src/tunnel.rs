//! Read-only macOS routing table inspection.

use std::time::Duration;

use tokio::{process::Command, time::timeout};

use crate::NetworkError;

const ROUTE_READ_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) async fn inspect() -> Result<bool, NetworkError> {
    let mut netstat = Command::new("/usr/sbin/netstat");
    netstat.args(["-rn"]).env("LC_ALL", "C").kill_on_drop(true);
    let output = timeout(ROUTE_READ_TIMEOUT, netstat.output())
        .await
        .map_err(|_| NetworkError::Io)?
        .map_err(|_| NetworkError::Io)?;
    if !output.status.success() {
        return Err(NetworkError::Platform);
    }
    let table = std::str::from_utf8(&output.stdout).map_err(|_| NetworkError::Platform)?;
    if !table.lines().any(|line| line.starts_with("Destination")) {
        return Err(NetworkError::Platform);
    }
    Ok(has_tunnel_route(table))
}

fn has_tunnel_route(table: &str) -> bool {
    table.lines().any(|line| {
        let mut fields = line.split_ascii_whitespace();
        let (Some(destination), Some(_), Some(flags), Some(interface)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return false;
        };
        let tunnel = ["utun", "tun", "tap", "ipsec"].iter().any(|prefix| {
            interface.strip_prefix(prefix).is_some_and(|unit| {
                !unit.is_empty() && unit.bytes().all(|byte| byte.is_ascii_digit())
            })
        });
        // macOS keeps interface-scoped defaults on idle utun interfaces.
        // Those do not change the ordinary system route for Internet traffic.
        tunnel
            && flags.contains('U')
            && !flags.contains(['I', 'R', 'B'])
            && matches!(
                destination,
                "default"
                    | "0/0"
                    | "0.0.0.0/0"
                    | "::/0"
                    | "0/1"
                    | "0.0.0.0/1"
                    | "128/1"
                    | "128.0/1"
                    | "128.0.0.0/1"
                    | "::/1"
                    | "8000::/1"
                    | "2000::/3"
            )
    })
}

#[cfg(test)]
mod tests {
    use super::has_tunnel_route;

    #[test]
    fn distinguishes_internet_tunnel_routes_from_idle_and_private_routes() {
        assert!(!has_tunnel_route(
            "default 192.0.2.1 UGScg en0\n\
             default fe80::%utun0 UGcIg utun0\n\
             100.64/10 link#17 UCS utun3\n\
             fe80::%utun3/64 fe80::1%utun3 UcI utun3"
        ));
        for route in [
            "default link#17 UGScg utun3",
            "128.0/1 link#17 UCS utun3",
            "0/1 link#17 UCS tun0",
            "2000::/3 fe80::1 UGSc ipsec1",
            "::/1 link#17 UCS utun3",
        ] {
            assert!(has_tunnel_route(route), "{route}");
        }
    }
}
