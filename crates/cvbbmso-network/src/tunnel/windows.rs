//! Read-only route and interface inspection through Windows IP Helper.

use std::{ptr, slice};

use windows_sys::Win32::{
    Foundation::{ERROR_FILE_NOT_FOUND, ERROR_NOT_FOUND, NO_ERROR},
    NetworkManagement::{
        IpHelper::{
            FreeMibTable, GetIfEntry2, GetIpForwardTable2, IF_TYPE_PROP_VIRTUAL, IF_TYPE_TUNNEL,
            IP_ADDRESS_PREFIX, MIB_IF_ROW2, MIB_IPFORWARD_ROW2, MIB_IPFORWARD_TABLE2,
        },
        Ndis::{IfOperStatusUp, NdisMediumIP, NdisMediumTunnel},
    },
    Networking::WinSock::{AF_INET, AF_INET6, AF_UNSPEC},
};

use crate::NetworkError;

struct RouteTable(ptr::NonNull<MIB_IPFORWARD_TABLE2>);

impl RouteTable {
    fn read() -> Result<Option<Self>, NetworkError> {
        let mut table = ptr::null_mut();
        // SAFETY: table is a valid, writable out pointer. IP Helper transfers
        // ownership of its allocation on success.
        match unsafe { GetIpForwardTable2(AF_UNSPEC, &raw mut table) } {
            NO_ERROR => ptr::NonNull::new(table)
                .map(|table| Some(Self(table)))
                .ok_or(NetworkError::Platform),
            ERROR_NOT_FOUND => Ok(None),
            _ => Err(NetworkError::Platform),
        }
    }
}

impl Drop for RouteTable {
    fn drop(&mut self) {
        // SAFETY: this is the live allocation returned by GetIpForwardTable2.
        unsafe { FreeMibTable(self.0.as_ptr().cast()) };
    }
}

pub(super) async fn inspect() -> Result<bool, NetworkError> {
    tokio::task::spawn_blocking(inspect_routes)
        .await
        .map_err(|_| NetworkError::Platform)?
}

fn inspect_routes() -> Result<bool, NetworkError> {
    let Some(table) = RouteTable::read()? else {
        return Ok(false);
    };
    let raw = table.0.as_ptr();
    // SAFETY: IP Helper initialized NumEntries rows at the Table offset. The
    // binding's single-element array models a flexible C array. The slice
    // borrows the allocation only until the end of this function.
    let routes: &[MIB_IPFORWARD_ROW2] = unsafe {
        slice::from_raw_parts((&raw const (*raw).Table).cast(), (*raw).NumEntries as usize)
    };
    for route in routes {
        if !is_internet_route(&route.DestinationPrefix) {
            continue;
        }
        let mut interface = MIB_IF_ROW2 {
            InterfaceLuid: route.InterfaceLuid,
            ..MIB_IF_ROW2::default()
        };
        // SAFETY: interface is writable and has its input interface LUID set.
        match unsafe { GetIfEntry2(&raw mut interface) } {
            NO_ERROR if is_tunnel(&interface) => return Ok(true),
            NO_ERROR | ERROR_FILE_NOT_FOUND | ERROR_NOT_FOUND => {}
            _ => return Err(NetworkError::Platform),
        }
    }
    Ok(false)
}

fn is_internet_route(prefix: &IP_ADDRESS_PREFIX) -> bool {
    // SAFETY: IP Helper initializes the sockaddr family, and the IPv6 view
    // below is read only when that family selects the IPv6 union field.
    unsafe {
        match (prefix.Prefix.si_family, prefix.PrefixLength) {
            (AF_INET | AF_INET6, 0 | 1) => true,
            (AF_INET6, 3) => prefix.Prefix.Ipv6.sin6_addr.u.Byte[0] == 0x20,
            _ => false,
        }
    }
}

fn is_tunnel(interface: &MIB_IF_ROW2) -> bool {
    if interface.OperStatus != IfOperStatusUp {
        return false;
    }
    if interface.Type == IF_TYPE_TUNNEL
        || interface.MediaType == NdisMediumTunnel
        || (interface.Type == IF_TYPE_PROP_VIRTUAL && interface.MediaType == NdisMediumIP)
    {
        return true;
    }
    // Some VPN adapters present as Ethernet to IP Helper, including TAP.
    let description = String::from_utf16_lossy(&interface.Description).to_ascii_lowercase();
    [
        "tap-windows",
        "wintun",
        "wireguard",
        "ovpn-dco",
        "openvpn data channel offload",
    ]
    .iter()
    .any(|name| description.contains(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::{
        NetworkManagement::Ndis::IfOperStatusDown,
        Networking::WinSock::{IN6_ADDR, IN6_ADDR_0, SOCKADDR_IN6},
    };

    #[test]
    fn a_virtual_ethernet_adapter_is_distinct_from_an_active_tunnel() {
        let mut adapter = MIB_IF_ROW2 {
            OperStatus: IfOperStatusUp,
            Type: IF_TYPE_PROP_VIRTUAL,
            ..MIB_IF_ROW2::default()
        };
        assert!(!is_tunnel(&adapter));
        adapter.MediaType = NdisMediumIP;
        assert!(is_tunnel(&adapter));
        adapter.OperStatus = IfOperStatusDown;
        assert!(!is_tunnel(&adapter));
        adapter.OperStatus = IfOperStatusUp;
        adapter.MediaType = 0;
        adapter.Description[..11]
            .copy_from_slice(&"TAP-Windows".encode_utf16().collect::<Vec<_>>());
        assert!(is_tunnel(&adapter));
    }

    #[test]
    fn recognizes_default_and_split_default_routes() {
        let mut prefix = IP_ADDRESS_PREFIX::default();
        prefix.Prefix.si_family = AF_INET;
        assert!(is_internet_route(&prefix));
        prefix.PrefixLength = 1;
        assert!(is_internet_route(&prefix));
        prefix.PrefixLength = 16;
        assert!(!is_internet_route(&prefix));
        prefix.Prefix.si_family = AF_INET6;
        prefix.PrefixLength = 0;
        assert!(is_internet_route(&prefix));
        let mut address = [0; 16];
        address[0] = 0x20;
        prefix.Prefix.Ipv6 = SOCKADDR_IN6 {
            sin6_family: AF_INET6,
            sin6_addr: IN6_ADDR {
                u: IN6_ADDR_0 { Byte: address },
            },
            ..SOCKADDR_IN6::default()
        };
        prefix.PrefixLength = 3;
        assert!(is_internet_route(&prefix));
        prefix.PrefixLength = 64;
        assert!(!is_internet_route(&prefix));
    }

    #[test]
    fn reads_the_host_route_table() {
        assert!(inspect_routes().is_ok());
    }
}
