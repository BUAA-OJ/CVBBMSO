//! WFP adapter for Windows.
//!
//! The filters live in a dynamic WFP session: the kernel removes every filter of
//! the session when the process that owns the engine handle ends, so an abnormal
//! exit cannot leave the machine without network access. [`recover`] therefore
//! has nothing to undo, and the watchdog only covers the macOS adapter.
#![allow(unsafe_code, reason = "WFP has no safe binding")]

use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::{
    FWP_ACTION_BLOCK, FWP_ACTION_PERMIT, FWP_ACTION_TYPE, FWP_CONDITION_VALUE0,
    FWP_CONDITION_VALUE0_0, FWP_MATCH_EQUAL, FWP_UINT8, FWP_UINT16, FWP_V4_ADDR_AND_MASK,
    FWP_V4_ADDR_MASK, FWP_V6_ADDR_AND_MASK, FWP_V6_ADDR_MASK, FWP_VALUE0, FWP_VALUE0_0,
    FWPM_ACTION0, FWPM_ACTION0_0, FWPM_CONDITION_IP_PROTOCOL, FWPM_CONDITION_IP_REMOTE_ADDRESS,
    FWPM_CONDITION_IP_REMOTE_PORT, FWPM_DISPLAY_DATA0, FWPM_FILTER_CONDITION0,
    FWPM_FILTER_FLAG_DISABLED, FWPM_FILTER0, FWPM_LAYER_ALE_AUTH_CONNECT_V4,
    FWPM_LAYER_ALE_AUTH_CONNECT_V6, FWPM_LAYER_OUTBOUND_TRANSPORT_V4,
    FWPM_LAYER_OUTBOUND_TRANSPORT_V6, FWPM_SESSION_FLAG_DYNAMIC, FWPM_SESSION0, FWPM_SUBLAYER0,
    FWPM_TXN_READ_ONLY, FwpmEngineClose0, FwpmEngineOpen0, FwpmFilterAdd0, FwpmFilterDeleteById0,
    FwpmFilterGetById0, FwpmFreeMemory0, FwpmSubLayerAdd0, FwpmTransactionAbort0,
    FwpmTransactionBegin0, FwpmTransactionCommit0,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::Rpc::RPC_C_AUTHN_WINNT;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::core::GUID;

use crate::NetworkError;
use crate::egress::backend::{GuardBackend, InstallResult};
use crate::egress::policy::{Plan, Transport};

/// Sublayer that owns the guard filters.
const SUBLAYER: GUID = GUID::from_u128(0x7d2a1c44_5f6b_4a17_9b7e_63a8d4f1c2b9);
/// Weight of the catch-all block; permits use a higher weight so they win.
const BLOCK_WEIGHT: u8 = 1;
/// Weight of every permit filter.
const PERMIT_WEIGHT: u8 = 5;
/// IP protocol numbers used in conditions.
const PROTOCOL_TCP: u8 = 6;
/// IP protocol numbers used in conditions.
const PROTOCOL_UDP: u8 = 17;
const ALE_LAYERS: [GUID; 2] = [
    FWPM_LAYER_ALE_AUTH_CONNECT_V4,
    FWPM_LAYER_ALE_AUTH_CONNECT_V6,
];
const TRANSPORT_LAYERS: [GUID; 2] = [
    FWPM_LAYER_OUTBOUND_TRANSPORT_V4,
    FWPM_LAYER_OUTBOUND_TRANSPORT_V6,
];
/// Loopback prefixes kept reachable for local IPC.
const LOOPBACK_V4: ([u8; 4], u8) = ([127, 0, 0, 1], 32);
/// Loopback prefixes kept reachable for local IPC.
const LOOPBACK_V6: ([u8; 16], u8) = ([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1], 128);

/// Installs the WFP filters and returns a handle to the owning session.
///
/// # Errors
///
/// [`NetworkError::PermissionDenied`] without administrator rights and
/// [`NetworkError::Platform`] when WFP rejects the session or a filter.
pub(crate) fn install(plan: &Plan) -> Result<InstallResult, NetworkError> {
    require_elevated()?;
    Ok(Arc::new(WfpGuard {
        session: Mutex::new(Some(Session::start(plan)?)),
    }))
}

pub(crate) fn prepare() -> Result<(), NetworkError> {
    require_elevated()
}

/// Handle to a running WFP session.
struct WfpGuard {
    session: Mutex<Option<Session>>,
}

impl WfpGuard {
    fn session(&self) -> MutexGuard<'_, Option<Session>> {
        self.session.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl GuardBackend for WfpGuard {
    fn apply(&self, plan: &Plan) -> Result<(), NetworkError> {
        self.session()
            .as_mut()
            .ok_or(NetworkError::Platform)?
            .replace(plan)
    }

    fn verify(&self) -> Result<(), NetworkError> {
        self.session()
            .as_ref()
            .ok_or(NetworkError::Platform)?
            .verify()
    }

    fn revert(&self) -> Result<(), NetworkError> {
        let mut slot = self.session();
        if let Some(session) = slot.as_mut() {
            session.engine.close()?;
            *slot = None;
        }
        Ok(())
    }
}

/// Filter IDs are assigned by BFE and identify immutable filter definitions.
struct Session {
    engine: Engine,
    filters: Vec<u64>,
    plan: Plan,
}

impl Session {
    fn start(plan: &Plan) -> Result<Self, NetworkError> {
        let engine = Engine::open()?;
        let filters = engine.transaction(0, || {
            engine.add_sublayer()?;
            engine.replace_filters(&[], plan)
        })?;
        let session = Self {
            engine,
            filters,
            plan: plan.clone(),
        };
        session.verify()?;
        Ok(session)
    }

    fn replace(&mut self, plan: &Plan) -> Result<(), NetworkError> {
        if self.plan != *plan {
            let next = self
                .engine
                .transaction(0, || self.engine.replace_filters(&self.filters, plan))?;
            self.filters = next;
            self.plan = plan.clone();
        }
        self.verify()
    }

    fn verify(&self) -> Result<(), NetworkError> {
        self.engine.transaction(FWPM_TXN_READ_ONLY, || {
            for &id in &self.filters {
                self.engine.verify_filter(id)?;
            }
            Ok(())
        })
    }
}

struct Engine(HANDLE);

// SAFETY: a WFP engine handle belongs to the process and has no thread affinity.
// WfpGuard serializes access, including transactions and closure, with its mutex.
unsafe impl Send for Engine {}

impl Engine {
    fn open() -> Result<Self, NetworkError> {
        let mut name = wide_name();
        let session = FWPM_SESSION0 {
            displayData: display_data(&mut name),
            flags: FWPM_SESSION_FLAG_DYNAMIC,
            txnWaitTimeoutInMSec: 5_000,
            ..FWPM_SESSION0::default()
        };
        let mut handle = std::ptr::null_mut();
        // SAFETY: the session and its name live through the call. WFP writes an
        // owned handle to the out parameter on success.
        check(unsafe {
            FwpmEngineOpen0(
                std::ptr::null(),
                RPC_C_AUTHN_WINNT,
                std::ptr::null(),
                &raw const session,
                &raw mut handle,
            )
        })?;
        if handle.is_null() {
            return Err(NetworkError::Platform);
        }
        Ok(Self(handle))
    }

    fn close(&mut self) -> Result<(), NetworkError> {
        if !self.0.is_null() {
            // SAFETY: self owns the open handle and has exclusive access to it.
            check(unsafe { FwpmEngineClose0(self.0) })?;
            self.0 = std::ptr::null_mut();
        }
        Ok(())
    }

    fn transaction<T>(
        &self,
        flags: u32,
        operation: impl FnOnce() -> Result<T, NetworkError>,
    ) -> Result<T, NetworkError> {
        // SAFETY: the engine is open and no other caller can start a
        // transaction while the owning guard is locked.
        check(unsafe { FwpmTransactionBegin0(self.0, flags) })?;
        let result = operation().and_then(|value| {
            // SAFETY: this is the transaction opened above.
            check(unsafe { FwpmTransactionCommit0(self.0) })?;
            Ok(value)
        });
        if result.is_err() {
            // SAFETY: abort discards all staged changes and cannot remove the
            // previous committed rules, even when the commit already failed.
            unsafe { FwpmTransactionAbort0(self.0) };
        }
        result
    }

    fn add_sublayer(&self) -> Result<(), NetworkError> {
        let mut name = wide_name();
        let sublayer = FWPM_SUBLAYER0 {
            subLayerKey: SUBLAYER,
            displayData: display_data(&mut name),
            weight: 0x8000,
            ..FWPM_SUBLAYER0::default()
        };
        // SAFETY: all input storage is valid until WFP has copied it.
        check(unsafe { FwpmSubLayerAdd0(self.0, &raw const sublayer, std::ptr::null_mut()) })
    }

    fn replace_filters(&self, previous: &[u64], plan: &Plan) -> Result<Vec<u64>, NetworkError> {
        let next = filter_specs(plan)
            .iter()
            .map(|spec| add_filter(self.0, spec))
            .collect::<Result<Vec<_>, _>>()?;
        for &id in previous {
            // SAFETY: the engine is open. The deletion is staged in the
            // same transaction as the replacement rules.
            check(unsafe { FwpmFilterDeleteById0(self.0, id) })?;
        }
        Ok(next)
    }

    fn verify_filter(&self, id: u64) -> Result<(), NetworkError> {
        let mut filter = std::ptr::null_mut();
        // SAFETY: the engine is open and filter points to a writable out parameter.
        check(unsafe { FwpmFilterGetById0(self.0, id, &raw mut filter) })?;
        if filter.is_null() {
            return Err(NetworkError::Platform);
        }
        // SAFETY: WFP returned an initialized filter owned by the caller; it
        // stays live until we release it with the WFP allocator below.
        let enabled = unsafe { (*filter).flags & FWPM_FILTER_FLAG_DISABLED == 0 };
        let mut allocation = filter.cast();
        // SAFETY: this is the allocation returned by FwpmFilterGetById0.
        unsafe { FwpmFreeMemory0(&raw mut allocation) };
        if enabled {
            Ok(())
        } else {
            Err(NetworkError::Platform)
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn wide_name() -> Vec<u16> {
    "CVBBMSO egress guard".encode_utf16().chain([0]).collect()
}

fn display_data(name: &mut [u16]) -> FWPM_DISPLAY_DATA0 {
    FWPM_DISPLAY_DATA0 {
        name: name.as_mut_ptr(),
        ..FWPM_DISPLAY_DATA0::default()
    }
}

fn check(status: u32) -> Result<(), NetworkError> {
    if status == 0 {
        Ok(())
    } else {
        Err(NetworkError::Platform)
    }
}

/// One filter to add.
struct FilterSpec {
    layer: GUID,
    weight: u8,
    action: FWP_ACTION_TYPE,
    conditions: Vec<ConditionSpec>,
}

/// One condition of a filter.
enum ConditionSpec {
    /// Matches an IPv4 address with a prefix length.
    AddressV4([u8; 4], u8),
    /// Matches an IPv6 address with a prefix length.
    AddressV6([u8; 16], u8),
    /// Matches an IP protocol number.
    Protocol(u8),
    /// Matches a remote port.
    Port(u16),
}

/// Adds one filter owned by the guard sublayer.
fn add_filter(engine: HANDLE, specification: &FilterSpec) -> Result<u64, NetworkError> {
    with_filter(specification, |filter| {
        let mut id = 0;
        // SAFETY: with_filter keeps every condition and value alive until
        // WFP has copied the filter.
        check(unsafe { FwpmFilterAdd0(engine, filter, std::ptr::null_mut(), &raw mut id) })?;
        Ok(id)
    })
}

/// Keeps the filter and all of its ABI backing storage alive for the call.
fn with_filter<T>(
    specification: &FilterSpec,
    use_filter: impl FnOnce(&FWPM_FILTER0) -> Result<T, NetworkError>,
) -> Result<T, NetworkError> {
    let owned = specification
        .conditions
        .iter()
        .map(OwnedCondition::new)
        .collect::<Vec<_>>();
    // The WFP ABI indexes an array of FWPM_FILTER_CONDITION0. The owning
    // wrappers have extra fields and therefore have a different stride.
    let mut conditions = owned
        .iter()
        .map(|entry| entry.condition)
        .collect::<Vec<_>>();
    let mut name = wide_name();
    let filter = FWPM_FILTER0 {
        displayData: display_data(&mut name),
        layerKey: specification.layer,
        subLayerKey: SUBLAYER,
        weight: weight_value(specification.weight),
        numFilterConditions: u32::try_from(conditions.len()).map_err(|_| NetworkError::Platform)?,
        filterCondition: if conditions.is_empty() {
            std::ptr::null_mut()
        } else {
            conditions.as_mut_ptr()
        },
        action: FWPM_ACTION0 {
            r#type: specification.action,
            Anonymous: FWPM_ACTION0_0 {
                filterType: GUID::from_u128(0),
            },
        },
        ..FWPM_FILTER0::default()
    };
    use_filter(&filter)
}

/// A condition whose pointed-to data is owned by the value.
struct OwnedCondition {
    condition: FWPM_FILTER_CONDITION0,
    _mask_v4: Option<Box<FWP_V4_ADDR_AND_MASK>>,
    _mask_v6: Option<Box<FWP_V6_ADDR_AND_MASK>>,
}

impl OwnedCondition {
    fn new(specification: &ConditionSpec) -> Self {
        match specification {
            ConditionSpec::AddressV4(octets, prefix) => {
                let mask = Box::new(FWP_V4_ADDR_AND_MASK {
                    // WFP takes the numeric address and mask in host byte order.
                    addr: u32::from_be_bytes(*octets),
                    mask: prefix_mask_v4(*prefix),
                });
                let mut condition =
                    condition_of(FWPM_CONDITION_IP_REMOTE_ADDRESS, FWP_V4_ADDR_MASK);
                condition.conditionValue.Anonymous = FWP_CONDITION_VALUE0_0 {
                    v4AddrMask: (&raw const *mask).cast_mut(),
                };
                Self {
                    condition,
                    _mask_v4: Some(mask),
                    _mask_v6: None,
                }
            }
            ConditionSpec::AddressV6(octets, prefix) => {
                let mask = Box::new(FWP_V6_ADDR_AND_MASK {
                    addr: *octets,
                    prefixLength: *prefix,
                });
                let mut condition =
                    condition_of(FWPM_CONDITION_IP_REMOTE_ADDRESS, FWP_V6_ADDR_MASK);
                condition.conditionValue.Anonymous = FWP_CONDITION_VALUE0_0 {
                    v6AddrMask: (&raw const *mask).cast_mut(),
                };
                Self {
                    condition,
                    _mask_v4: None,
                    _mask_v6: Some(mask),
                }
            }
            ConditionSpec::Protocol(protocol) => {
                let mut condition = condition_of(FWPM_CONDITION_IP_PROTOCOL, FWP_UINT8);
                condition.conditionValue.Anonymous = FWP_CONDITION_VALUE0_0 { uint8: *protocol };
                Self {
                    condition,
                    _mask_v4: None,
                    _mask_v6: None,
                }
            }
            ConditionSpec::Port(port) => {
                let mut condition = condition_of(FWPM_CONDITION_IP_REMOTE_PORT, FWP_UINT16);
                condition.conditionValue.Anonymous = FWP_CONDITION_VALUE0_0 { uint16: *port };
                Self {
                    condition,
                    _mask_v4: None,
                    _mask_v6: None,
                }
            }
        }
    }
}

/// Builds an equal-match condition with no value set yet.
fn condition_of(field: GUID, value_type: i32) -> FWPM_FILTER_CONDITION0 {
    FWPM_FILTER_CONDITION0 {
        fieldKey: field,
        matchType: FWP_MATCH_EQUAL,
        conditionValue: FWP_CONDITION_VALUE0 {
            r#type: value_type,
            Anonymous: FWP_CONDITION_VALUE0_0 {
                uint64: std::ptr::null_mut(),
            },
        },
    }
}

/// Builds the weight value of a filter.
fn weight_value(weight: u8) -> FWP_VALUE0 {
    FWP_VALUE0 {
        r#type: FWP_UINT8,
        Anonymous: FWP_VALUE0_0 { uint8: weight },
    }
}

/// Returns the IPv4 mask for a prefix length.
fn prefix_mask_v4(prefix: u8) -> u32 {
    u32::MAX
        .checked_shl(32 - u32::from(prefix.min(32)))
        .unwrap_or(0)
}

/// Returns the complete filter list for `plan`.
fn filter_specs(plan: &Plan) -> Vec<FilterSpec> {
    // Receiving a TCP SYN is authorized at RECV_ACCEPT, so its SYN-ACK and
    // later replies need per-packet filtering even when ALE_CONNECT is absent.
    let mut specifications = Vec::new();
    for layer in ALE_LAYERS {
        specifications.push(block(layer, Vec::new()));
    }
    // Keep the platform's IPv6 control traffic, including neighbor detection
    // to on-link unicast addresses, outside the new per-packet checks.
    for layer in TRANSPORT_LAYERS {
        for protocol in [PROTOCOL_TCP, PROTOCOL_UDP] {
            specifications.push(block(layer, vec![ConditionSpec::Protocol(protocol)]));
        }
    }
    specifications.extend(layer_permits(plan, ALE_LAYERS, &[None]));
    specifications.extend(layer_permits(
        plan,
        TRANSPORT_LAYERS,
        &[Some(PROTOCOL_TCP), Some(PROTOCOL_UDP)],
    ));
    specifications
}

fn block(layer: GUID, conditions: Vec<ConditionSpec>) -> FilterSpec {
    FilterSpec {
        layer,
        weight: BLOCK_WEIGHT,
        action: FWP_ACTION_BLOCK,
        conditions,
    }
}

fn layer_permits(
    plan: &Plan,
    layers: [GUID; 2],
    loopback_protocols: &[Option<u8>],
) -> Vec<FilterSpec> {
    let [ipv4, ipv6] = layers;
    // Loopback for local IPC.
    let mut specifications = Vec::new();
    for protocol in loopback_protocols {
        let conditions = || {
            protocol
                .iter()
                .copied()
                .map(ConditionSpec::Protocol)
                .collect()
        };
        specifications.push(permit(
            ipv4,
            ConditionSpec::AddressV4(LOOPBACK_V4.0, LOOPBACK_V4.1),
            conditions(),
        ));
        specifications.push(permit(
            ipv6,
            ConditionSpec::AddressV6(LOOPBACK_V6.0, LOOPBACK_V6.1),
            conditions(),
        ));
        specifications.push(permit(
            ipv6,
            ConditionSpec::AddressV6(
                std::net::Ipv4Addr::from(LOOPBACK_V4.0)
                    .to_ipv6_mapped()
                    .octets(),
                128,
            ),
            conditions(),
        ));
    }

    for service in &plan.services {
        let protocol = match service.transport {
            Transport::Tcp => PROTOCOL_TCP,
            Transport::Udp => PROTOCOL_UDP,
        };
        specifications.extend(permit_address(
            layers,
            &service.remote.ip(),
            service.remote.port(),
            protocol,
        ));
    }

    for target in [&plan.accoding, &plan.server] {
        for address in &target.addresses {
            for protocol in [PROTOCOL_TCP, PROTOCOL_UDP] {
                specifications.extend(permit_address(layers, address, target.port, protocol));
            }
        }
    }

    for address in &plan.certificate {
        specifications.extend(permit_address(
            layers,
            &address.ip(),
            address.port(),
            PROTOCOL_TCP,
        ));
    }

    specifications
}

/// Dual-stack sockets can be classified with an IPv4-mapped address at ALE.
fn permit_address(
    [ipv4, ipv6]: [GUID; 2],
    address: &IpAddr,
    port: u16,
    protocol: u8,
) -> impl Iterator<Item = FilterSpec> {
    let conditions = || vec![ConditionSpec::Protocol(protocol), ConditionSpec::Port(port)];
    let v4 = match address {
        IpAddr::V4(v4) => Some(permit(
            ipv4,
            ConditionSpec::AddressV4(v4.octets(), 32),
            conditions(),
        )),
        IpAddr::V6(_) => None,
    };
    let v6 = match address {
        IpAddr::V4(v4) => v4.to_ipv6_mapped(),
        IpAddr::V6(v6) => *v6,
    };
    v4.into_iter().chain([permit(
        ipv6,
        ConditionSpec::AddressV6(v6.octets(), 128),
        conditions(),
    )])
}

fn permit(layer: GUID, address: ConditionSpec, mut conditions: Vec<ConditionSpec>) -> FilterSpec {
    conditions.insert(0, address);
    FilterSpec {
        layer,
        weight: PERMIT_WEIGHT,
        // This is a soft permit. It cannot override a host firewall block.
        action: FWP_ACTION_PERMIT,
        conditions,
    }
}

/// Fails unless the process runs with administrator rights.
fn require_elevated() -> Result<(), NetworkError> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: the current process pseudo handle is always valid and `token`
    // receives a handle that is closed below.
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) };
    if opened == 0 {
        return Err(NetworkError::Platform);
    }

    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0u32;
    // SAFETY: `elevation` is the correct type for `TokenElevation` and `token`
    // is an open token handle.
    let queried = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&raw mut elevation).cast(),
            u32::try_from(std::mem::size_of::<TOKEN_ELEVATION>()).unwrap_or(0),
            &raw mut returned,
        )
    };
    // SAFETY: the token handle was opened above and is closed exactly once.
    unsafe { CloseHandle(token) };

    if queried == 0 {
        return Err(NetworkError::Platform);
    }
    if elevation.TokenIsElevated == 0 {
        return Err(NetworkError::PermissionDenied);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::policy::{Plan, Target};
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn plan() -> Plan {
        Plan::new(
            Target::new(vec![IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10))], 443),
            Target::new(vec!["2001:db8::10".parse().unwrap()], 8443),
            vec!["203.0.113.20:80".parse().expect("certificate responder")],
            Vec::new(),
        )
    }

    #[test]
    fn a_block_filter_exists_for_both_families_at_connect_and_transport() {
        let specifications = filter_specs(&plan());
        for layer in ALE_LAYERS {
            assert!(specifications.iter().any(|specification| {
                same_layer(specification.layer, layer)
                    && specification.action == FWP_ACTION_BLOCK
                    && specification.conditions.is_empty()
            }));
        }
        for layer in TRANSPORT_LAYERS {
            let blocks = specifications
                .iter()
                .filter(|spec| same_layer(spec.layer, layer) && spec.action == FWP_ACTION_BLOCK)
                .collect::<Vec<_>>();
            assert_eq!(blocks.len(), 2);
            for protocol in [PROTOCOL_TCP, PROTOCOL_UDP] {
                assert!(blocks.iter().any(|spec| matches!(
                    &spec.conditions[..], [ConditionSpec::Protocol(found)] if *found == protocol
                )));
            }
        }
    }

    #[test]
    fn permits_use_a_higher_weight_than_the_block() {
        let specifications = filter_specs(&plan());
        assert!(
            specifications
                .iter()
                .all(|specification| specification.weight == BLOCK_WEIGHT
                    || specification.weight == PERMIT_WEIGHT)
        );
        const { assert!(BLOCK_WEIGHT < PERMIT_WEIGHT) };
    }

    #[test]
    fn transport_replies_match_the_same_remote_address_port_and_protocol() {
        let specifications = filter_specs(&plan());
        let v6 = "2001:db8::10".parse::<Ipv6Addr>().unwrap().octets();
        for protocol in [PROTOCOL_TCP, PROTOCOL_UDP] {
            assert!(specifications.iter().any(|spec| same_layer(spec.layer, TRANSPORT_LAYERS[0])
                && matches!(&spec.conditions[..], [ConditionSpec::AddressV4([203, 0, 113, 10], 32), ConditionSpec::Protocol(found), ConditionSpec::Port(443)] if *found == protocol)));
            assert!(specifications.iter().any(|spec| same_layer(spec.layer, TRANSPORT_LAYERS[1])
                && matches!(&spec.conditions[..], [ConditionSpec::AddressV6(address, 128), ConditionSpec::Protocol(found), ConditionSpec::Port(8443)] if *found == protocol && *address == v6)));
        }
        let refreshed = filter_specs(&Plan::default());
        assert!(!refreshed.iter().any(|spec| matches!(
            &spec.conditions[..],
            [
                _,
                ConditionSpec::Protocol(PROTOCOL_TCP | PROTOCOL_UDP),
                ConditionSpec::Port(443 | 8443)
            ]
        )));
    }

    #[test]
    fn transport_leaves_neighbor_discovery_to_the_host_and_ale_is_unchanged() {
        let specs = filter_specs(&plan());
        for (layer, expected) in [
            (TRANSPORT_LAYERS[1], None),
            (ALE_LAYERS[1], Some(FWP_ACTION_BLOCK)),
        ] {
            for address in [
                "2001:db8::10",
                "fd00::10",
                "fe80::10",
                "ff02::1:ff00:10",
                "2001:db8:1::10",
            ] {
                let address = address.parse::<Ipv6Addr>().unwrap().octets();
                let matched = specs
                    .iter()
                    .filter(|spec| {
                        same_layer(spec.layer, layer)
                            && spec.conditions.iter().all(|condition| match condition {
                                ConditionSpec::Protocol(protocol) => *protocol == 58,
                                ConditionSpec::AddressV6(destination, 128) => {
                                    *destination == address
                                }
                                _ => false,
                            })
                    })
                    .max_by_key(|spec| spec.weight);
                assert_eq!(matched.map(|spec| spec.action), expected);
            }
        }
    }

    fn same_layer(a: GUID, b: GUID) -> bool {
        (a.data1, a.data2, a.data3, a.data4) == (b.data1, b.data2, b.data3, b.data4)
    }

    #[test]
    fn ffi_sees_all_three_conditions_in_order() {
        let filter = permit_address(
            TRANSPORT_LAYERS,
            &"203.0.113.10".parse().unwrap(),
            8443,
            PROTOCOL_TCP,
        )
        .next()
        .unwrap();
        with_filter(&filter, |raw| {
            assert!(!raw.displayData.name.is_null());
            assert_eq!(raw.numFilterConditions, 3);
            assert!(same_layer(raw.layerKey, TRANSPORT_LAYERS[0]));
            assert_eq!(raw.flags & windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_FILTER_FLAG_CLEAR_ACTION_RIGHT, 0);
            // SAFETY: with_filter keeps the array and its data live. The type
            // assertions select the correct union member for each condition.
            unsafe {
                let conditions = std::slice::from_raw_parts(raw.filterCondition, 3);
                let [address, protocol, port] = conditions else {
                    unreachable!()
                };
                assert_eq!(address.conditionValue.r#type, FWP_V4_ADDR_MASK);
                assert_eq!(protocol.conditionValue.r#type, FWP_UINT8);
                assert_eq!(port.conditionValue.r#type, FWP_UINT16);
                let mask = &*address.conditionValue.Anonymous.v4AddrMask;
                assert_eq!(mask.addr, 0xcb00_710a);
                assert_eq!(mask.mask, u32::MAX);
                assert_eq!(protocol.conditionValue.Anonymous.uint8, PROTOCOL_TCP);
                assert_eq!(port.conditionValue.Anonymous.uint16, 8443);
            }
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn prefix_masks_cover_the_address() {
        assert_eq!(prefix_mask_v4(32), 0xffff_ffff);
        assert_eq!(prefix_mask_v4(24), 0xffff_ff00);
        assert_eq!(prefix_mask_v4(8), 0xff00_0000);
        assert_eq!(prefix_mask_v4(0), 0);
    }
}
