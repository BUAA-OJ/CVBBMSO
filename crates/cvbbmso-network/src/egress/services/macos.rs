//! Effective DNS configuration, DHCP lease information and network time.

use crate::NetworkError;
use crate::egress::commands::{Commands, SystemCommands};
use crate::egress::policy::Transport;

use super::{Services, parse_address};

pub(super) fn discover() -> Result<Services, NetworkError> {
    let commands = SystemCommands::new(None);
    let mut services = Services::default();
    let dns = commands.output("/usr/sbin/scutil", &["--dns"])?;
    add_dns(&mut services, &dns);

    let interfaces = commands.output("/sbin/ifconfig", &["-l", "-u"])?;
    for name in interfaces.split_whitespace() {
        if !name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
            || !name.bytes().all(|byte| byte.is_ascii_alphanumeric())
        {
            continue;
        }
        if let Ok(lease) = commands.output("/usr/sbin/ipconfig", &["getpacket", name]) {
            for line in lease.lines() {
                if let Some(address) = line
                    .trim()
                    .strip_prefix("server_identifier (ip): ")
                    .and_then(parse_address)
                {
                    services.add(address, 67, Transport::Udp);
                }
            }
        }
    }

    if commands
        .output("/usr/sbin/systemsetup", &["-getusingnetworktime"])?
        .lines()
        .any(|line| line.trim() == "Network Time: On")
    {
        let time = commands.output("/usr/sbin/systemsetup", &["-getnetworktimeserver"])?;
        services.time_hosts.extend(
            time.lines()
                .filter_map(|line| line.trim().strip_prefix("Network Time Server: "))
                .flat_map(str::split_ascii_whitespace)
                .map(str::to_owned),
        );
    }
    Ok(services)
}

fn add_dns(services: &mut Services, output: &str) {
    for entry in output.split("resolver #").skip(1) {
        let fields = entry
            .lines()
            .filter_map(|line| line.trim().split_once(':'))
            .map(|(key, value)| (key.trim(), value.trim()))
            .collect::<Vec<_>>();
        let options = fields
            .iter()
            .find(|(key, _)| *key == "options")
            .map_or("", |(_, value)| *value);
        let protocol = options
            .split_ascii_whitespace()
            .find(|value| matches!(*value, "doh" | "dot" | "mdns"));
        if protocol == Some("mdns") {
            continue;
        }
        let port = fields
            .iter()
            .find(|(key, _)| *key == "port")
            .and_then(|(_, port)| port.parse().ok())
            .unwrap_or(match protocol {
                Some("doh") => 443,
                Some("dot") => 853,
                _ => 53,
            });
        for (_, value) in fields
            .iter()
            .filter(|(key, _)| key.starts_with("nameserver["))
        {
            if let Some(address) = parse_address(value) {
                match (protocol, port) {
                    (Some("doh" | "dot"), _) | (_, 853) => {
                        services.add(address, port, Transport::Tcp);
                    }
                    _ => services.add_plain_dns(address, port),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_resolver_only_gets_its_own_protocol_and_port() {
        let mut services = Services::default();
        add_dns(
            &mut services,
            "
resolver #1
  nameserver[0] : 192.0.2.53
resolver #2
  options : dot
  nameserver[0] : 2001:db8::53
  port : 8853
resolver #3
  options : mdns
  nameserver[0] : 224.0.0.251
",
        );
        assert_eq!(services.rules.len(), 3);
        assert_eq!(services.rules[0].remote.to_string(), "192.0.2.53:53");
        assert_eq!(services.rules[0].transport, Transport::Tcp);
        assert_eq!(services.rules[1].transport, Transport::Udp);
        assert_eq!(services.rules[2].remote.to_string(), "[2001:db8::53]:8853");
        assert_eq!(services.rules[2].transport, Transport::Tcp);
    }
}
