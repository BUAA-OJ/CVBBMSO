//! DNS and DHCP destinations come from active Windows adapter configuration.
#![allow(
    unsafe_code,
    reason = "IP Helper exposes socket addresses through Win32 pointers"
)]

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::mem::{MaybeUninit, size_of};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::process::Command;

use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_NO_DATA};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_MULTICAST, GAA_FLAG_SKIP_UNICAST, GetAdaptersAddresses,
    IP_ADAPTER_ADDRESSES_LH, IP_ADAPTER_DHCP_ENABLED,
};
use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows_sys::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6, SOCKET_ADDRESS,
};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

use super::{Services, parse_address};
use crate::NetworkError;
use crate::egress::policy::Transport;

// Use only the system module and the two W32Time configuration keys. Output
// consists of records we control, independent of the Windows display language.
const CONFIG_SCRIPT: &str = r"
$ErrorActionPreference = 'Stop'
$env:PSModulePath = [IO.Path]::Combine($PSHOME, 'Modules')
if (Get-Command Get-DnsClientDohServerAddress -ErrorAction SilentlyContinue) {
    Get-DnsClientDohServerAddress | Where-Object AutoUpgrade | ForEach-Object {
        [Console]::WriteLine(('DNS|{0}|{1}|{2}|{3}' -f $_.ServerAddress,$_.DohTemplate,$_.DotHost,$_.AllowFallbackToUdp))
    }
}
$policy = Get-ItemProperty 'HKLM:\SOFTWARE\Policies\Microsoft\W32Time\TimeProviders\NtpClient' -ErrorAction SilentlyContinue
$time = Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Services\W32Time\Parameters' -ErrorAction SilentlyContinue
$type = if ($policy.Type) { $policy.Type } else { $time.Type }
if ($type -in @('NTP','AllSync')) {
    $peers = if ($policy.NtpServer) { $policy.NtpServer } else { $time.NtpServer }
    $peers -split '\s+' | ForEach-Object {
        if ($_) { [Console]::WriteLine(('TIME|{0}' -f (($_ -split ',',2)[0]))) }
    }
}
";

pub(super) fn discover() -> Result<Services, NetworkError> {
    let mut services = Services::default();
    let dns = read_adapters(&mut services)?;
    let system = system_directory()?;
    let output = Command::new(system.join(r"WindowsPowerShell\v1.0\powershell.exe"))
        .current_dir(&system)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            CONFIG_SCRIPT,
        ])
        .output()
        .map_err(|_| NetworkError::Platform)?;
    if !output.status.success() {
        return Err(NetworkError::Platform);
    }
    add_config(
        &mut services,
        &dns,
        &String::from_utf8_lossy(&output.stdout),
    );

    // Domain members can sync with a peer selected by W32Time at runtime.
    if let Ok(output) = Command::new(system.join("w32tm.exe"))
        .current_dir(&system)
        .args(["/query", "/source"])
        .output()
        && output.status.success()
    {
        add_time_host(
            &mut services,
            String::from_utf8_lossy(&output.stdout).trim(),
        );
    }
    Ok(services)
}

fn add_config(services: &mut Services, dns: &BTreeSet<IpAddr>, output: &str) {
    let mut plaintext = dns.clone();
    for record in output.lines() {
        let fields = record.split('|').collect::<Vec<_>>();
        match fields.as_slice() {
            ["DNS", address, https, tls, fallback] => {
                let Some(address) = parse_address(address).filter(|address| dns.contains(address))
                else {
                    continue;
                };
                let port = tls
                    .rsplit_once(':')
                    .and_then(|(_, port)| port.parse().ok())
                    .or_else(|| {
                        url::Url::parse(https)
                            .ok()
                            .filter(|url| url.scheme() == "https")
                            .and_then(|url| url.port_or_known_default())
                    });
                if let Some(port) = port {
                    services.add(address, port, Transport::Tcp);
                    if fallback.eq_ignore_ascii_case("false") {
                        plaintext.remove(&address);
                    }
                }
            }
            ["TIME", host] => add_time_host(services, host),
            _ => {}
        }
    }
    for address in plaintext {
        services.add_plain_dns(address, 53);
    }
}

fn add_time_host(services: &mut Services, host: &str) {
    if let Some(address) = parse_address(host) {
        services.add(address, 123, Transport::Udp);
    } else if !host.is_empty()
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-".contains(&byte))
    {
        services.time_hosts.push(host.to_owned());
    }
}

fn system_directory() -> Result<PathBuf, NetworkError> {
    let mut buffer = [0; 260];
    // SAFETY: buffer has capacity for the supplied length, in UTF-16 units.
    let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), 260) } as usize;
    if length == 0 || length >= buffer.len() {
        return Err(NetworkError::Platform);
    }
    Ok(PathBuf::from(OsString::from_wide(&buffer[..length])))
}

fn read_adapters(services: &mut Services) -> Result<BTreeSet<IpAddr>, NetworkError> {
    const FLAGS: u32 = GAA_FLAG_SKIP_UNICAST | GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST;
    let mut size = 15_000;
    for _ in 0..3 {
        let capacity = (size as usize).div_ceil(size_of::<IP_ADAPTER_ADDRESSES_LH>());
        let mut storage = vec![MaybeUninit::<IP_ADAPTER_ADDRESSES_LH>::uninit(); capacity];
        let head = storage.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        // SAFETY: head points to aligned, writable storage of at least size
        // bytes. The function owns initialization of the returned linked lists.
        let status = unsafe {
            GetAdaptersAddresses(
                u32::from(AF_UNSPEC),
                FLAGS,
                std::ptr::null(),
                head,
                &raw mut size,
            )
        };
        match status {
            ERROR_BUFFER_OVERFLOW => continue,
            ERROR_NO_DATA => return Ok(BTreeSet::new()),
            0 => {}
            _ => return Err(NetworkError::Platform),
        }
        let mut resolvers = BTreeSet::new();
        let mut next = head;
        while !next.is_null() {
            // SAFETY: this node and its child list are from the successful OS
            // call, and storage remains live during every lookup.
            let adapter = unsafe { &*next };
            next = adapter.Next;
            if adapter.OperStatus != IfOperStatusUp {
                continue;
            }
            // SAFETY: documented flags view of the adapter's flags union.
            let dhcp = unsafe { adapter.Anonymous2.Flags } & IP_ADAPTER_DHCP_ENABLED != 0;
            if dhcp {
                // SAFETY: this socket address belongs to the live adapter buffer.
                if let Some(address @ IpAddr::V4(_)) = unsafe { ip(adapter.Dhcpv4Server) } {
                    services.add(address, 67, Transport::Udp);
                }
            }
            let mut dns = adapter.FirstDnsServerAddress;
            while !dns.is_null() {
                // SAFETY: each DNS node and address are in the live adapter buffer.
                let node = unsafe { &*dns };
                if let Some(address) = unsafe { ip(node.Address) } {
                    resolvers.insert(address);
                }
                dns = node.Next;
            }
        }
        return Ok(resolvers);
    }
    Err(NetworkError::Platform)
}

// Caller guarantees that a non-null socket address points into the live
// GetAdaptersAddresses buffer. The family and length select its concrete type.
unsafe fn ip(address: SOCKET_ADDRESS) -> Option<IpAddr> {
    let socket = address.lpSockaddr;
    let size = usize::try_from(address.iSockaddrLength).ok()?;
    if socket.is_null() || size < size_of::<SOCKADDR>() {
        return None;
    }
    // SAFETY: socket is valid under the caller's contract and large enough to
    // read the family. Each arm checks the full size before reading the union.
    unsafe {
        match (*socket).sa_family {
            AF_INET if size >= size_of::<SOCKADDR_IN>() => {
                let address = socket.cast::<SOCKADDR_IN>().read_unaligned();
                Some(Ipv4Addr::from(address.sin_addr.S_un.S_addr.to_ne_bytes()).into())
            }
            AF_INET6 if size >= size_of::<SOCKADDR_IN6>() => {
                let address = socket.cast::<SOCKADDR_IN6>().read_unaligned();
                Some(Ipv6Addr::from(address.sin6_addr.u.Byte).into())
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secure_dns_uses_only_active_resolvers_and_configured_ports() {
        let mut services = Services::default();
        let resolvers = BTreeSet::from([
            "192.0.2.53".parse().unwrap(),
            "2001:db8::53".parse().unwrap(),
        ]);
        add_config(
            &mut services,
            &resolvers,
            concat!(
                "DNS|192.0.2.53|https://example.test:8443/dns-query||False\n",
                "DNS|192.0.2.99|https://unused.example/dns-query||False\n",
                "TIME|192.0.2.123\n",
            ),
        );
        let rules = services
            .rules
            .iter()
            .map(|rule| (rule.remote.to_string(), rule.transport))
            .collect::<Vec<_>>();
        assert_eq!(
            rules,
            [
                ("192.0.2.53:8443".into(), Transport::Tcp),
                ("192.0.2.123:123".into(), Transport::Udp),
                ("[2001:db8::53]:53".into(), Transport::Tcp),
                ("[2001:db8::53]:53".into(), Transport::Udp),
            ]
        );
    }
}
