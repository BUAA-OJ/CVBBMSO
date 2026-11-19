//! WFP adapter for Windows.
//!
//! The filters live in a dynamic WFP session: the kernel removes every filter of
//! the session when the process that owns the engine handle ends, so an abnormal
//! exit cannot leave the machine without network access. [`recover`] therefore
//! has nothing to undo, and the watchdog only covers the macOS adapter.
#![allow(unsafe_code, reason = "WFP has no safe binding")]

use std::net::IpAddr;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::{
    FWP_ACTION_BLOCK, FWP_ACTION_PERMIT, FWP_ACTION_TYPE, FWP_CONDITION_VALUE0,
    FWP_CONDITION_VALUE0_0, FWP_MATCH_EQUAL, FWP_UINT8, FWP_UINT16, FWP_V4_ADDR_AND_MASK,
    FWP_V4_ADDR_MASK, FWP_V6_ADDR_AND_MASK, FWP_V6_ADDR_MASK, FWP_VALUE0, FWP_VALUE0_0,
    FWPM_ACTION0, FWPM_ACTION0_0, FWPM_CONDITION_IP_PROTOCOL, FWPM_CONDITION_IP_REMOTE_ADDRESS,
    FWPM_CONDITION_IP_REMOTE_PORT, FWPM_FILTER_CONDITION0, FWPM_FILTER0, FWPM_FILTER0_0,
    FWPM_LAYER_ALE_AUTH_CONNECT_V4, FWPM_LAYER_ALE_AUTH_CONNECT_V6, FWPM_SESSION_FLAG_DYNAMIC,
    FWPM_SESSION0, FWPM_SUBLAYER0, FwpmEngineClose0, FwpmEngineOpen0, FwpmFilterAdd0,
    FwpmSubLayerAdd0,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::core::GUID;

use crate::NetworkError;
use crate::egress::backend::{GuardBackend, InstallResult};
use crate::egress::policy::Plan;

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
/// Datagram services an exam client needs: DNS, DHCP and NTP.
const DATAGRAM_PORTS: [u16; 5] = [53, 67, 68, 123, 123];
/// Ports used to fetch certificate status.
const CERTIFICATE_PORTS: [u16; 2] = [80, 443];
/// Loopback prefixes kept reachable for local IPC.
const LOOPBACK_V4: ([u8; 4], u8) = ([127, 0, 0, 0], 8);
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

/// Reports success because the dynamic session cleans itself up.
///
/// # Errors
///
/// Never returns an error on this target.
pub(crate) fn recover() -> Result<(), NetworkError> {
    Ok(())
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
        let mut slot = self.session();
        if let Some(session) = slot.take() {
            session.stop();
        }
        *slot = Some(Session::start(plan)?);
        Ok(())
    }

    fn verify(&self) -> Result<(), NetworkError> {
        // Filters belong to the session, which exists exactly as long as the
        // owning thread keeps the engine open.
        if self.session().is_some() {
            Ok(())
        } else {
            Err(NetworkError::Platform)
        }
    }

    fn revert(&self) -> Result<(), NetworkError> {
        if let Some(session) = self.session().take() {
            session.stop();
        }
        Ok(())
    }
}

/// A WFP session owned by a thread that keeps the engine handle open.
struct Session {
    stop: Sender<()>,
    join: JoinHandle<()>,
}

impl Session {
    fn start(plan: &Plan) -> Result<Self, NetworkError> {
        let (ready_sender, ready_receiver) = channel::<Result<(), NetworkError>>();
        let (stop_sender, stop_receiver) = channel::<()>();
        let plan = plan.clone();

        let join = std::thread::spawn(move || {
            match install_filters(&plan) {
                Ok(engine) => {
                    let _ = ready_sender.send(Ok(()));
                    // The session ends with the engine handle; closing it removes
                    // every filter created in this session.
                    let _ = stop_receiver.recv();
                    // SAFETY: `engine` is an open engine handle owned by this
                    // thread and is closed exactly once here.
                    unsafe { FwpmEngineClose0(engine) };
                }
                Err(error) => {
                    let _ = ready_sender.send(Err(error));
                }
            }
        });

        match ready_receiver.recv() {
            Ok(Ok(())) => Ok(Self {
                stop: stop_sender,
                join,
            }),
            Ok(Err(error)) => {
                let _ = join.join();
                Err(error)
            }
            Err(_) => Err(NetworkError::Platform),
        }
    }

    fn stop(self) {
        let _ = self.stop.send(());
        let _ = self.join.join();
    }
}

/// Opens a dynamic session, adds the sublayer and every filter.
///
/// # Safety
///
/// Callers must treat the returned handle as owned by the calling thread and
/// close it with `FwpmEngineClose0`.
fn install_filters(plan: &Plan) -> Result<HANDLE, NetworkError> {
    let mut session = FWPM_SESSION0::default();
    session.flags = FWPM_SESSION_FLAG_DYNAMIC;

    let mut engine: HANDLE = std::ptr::null_mut();
    // SAFETY: the session structure is valid for the call and `engine` receives
    // the handle; no borrowed data escapes.
    let status = unsafe {
        FwpmEngineOpen0(
            std::ptr::null(),
            0,
            std::ptr::null(),
            &raw const session,
            &raw mut engine,
        )
    };
    if status != 0 || engine.is_null() {
        return Err(NetworkError::Platform);
    }

    let mut sublayer = FWPM_SUBLAYER0::default();
    sublayer.subLayerKey = SUBLAYER;
    sublayer.weight = 0x8000;
    // SAFETY: the sublayer is initialized and the engine handle is valid.
    let status = unsafe { FwpmSubLayerAdd0(engine, &raw const sublayer, std::ptr::null_mut()) };
    if status != 0 {
        // SAFETY: the engine handle was opened above and is still open.
        unsafe { FwpmEngineClose0(engine) };
        return Err(NetworkError::Platform);
    }

    for specification in filter_specs(plan) {
        if let Err(error) = add_filter(engine, &specification) {
            // SAFETY: closing the dynamic session removes every filter of it.
            unsafe { FwpmEngineClose0(engine) };
            return Err(error);
        }
    }

    Ok(engine)
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
fn add_filter(engine: HANDLE, specification: &FilterSpec) -> Result<(), NetworkError> {
    let mut owned = specification
        .conditions
        .iter()
        .map(OwnedCondition::new)
        .collect::<Vec<_>>();

    let mut filter = FWPM_FILTER0::default();
    filter.layerKey = specification.layer;
    filter.subLayerKey = SUBLAYER;
    filter.weight = weight_value(specification.weight);
    filter.numFilterConditions = u32::try_from(owned.len()).unwrap_or(0);
    filter.filterCondition = owned.as_mut_ptr().cast::<FWPM_FILTER_CONDITION0>();
    filter.action = FWPM_ACTION0 {
        r#type: specification.action,
        Anonymous: FWPM_ACTION0_0 {
            filterType: GUID::from_u128(0),
        },
    };
    filter.Anonymous = FWPM_FILTER0_0 {
        providerContextKey: GUID::from_u128(0),
    };

    // SAFETY: `owned` keeps every condition value alive for the duration of the
    // call, and WFP copies the data it needs before returning.
    let status = unsafe {
        FwpmFilterAdd0(
            engine,
            &raw const filter,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(NetworkError::Platform)
    }
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
                    // WFP expects the address and mask in network byte order.
                    addr: u32::from_ne_bytes(*octets),
                    mask: u32::from_ne_bytes(prefix_mask_v4(*prefix)),
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
fn prefix_mask_v4(prefix: u8) -> [u8; 4] {
    let prefix = prefix.min(32);
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    };
    mask.to_ne_bytes()
}

/// Returns the complete filter list for `plan`.
fn filter_specs(plan: &Plan) -> Vec<FilterSpec> {
    let mut specifications = Vec::new();

    for layer in [
        FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        FWPM_LAYER_ALE_AUTH_CONNECT_V6,
    ] {
        specifications.push(FilterSpec {
            layer,
            weight: BLOCK_WEIGHT,
            action: FWP_ACTION_BLOCK,
            conditions: Vec::new(),
        });
    }

    // Loopback for local IPC.
    specifications.push(permit_v4(LOOPBACK_V4.0, LOOPBACK_V4.1, Vec::new()));
    specifications.push(permit_v6(LOOPBACK_V6.0, LOOPBACK_V6.1, Vec::new()));

    // Datagram services: DNS, DHCP and NTP.
    for port in DATAGRAM_PORTS {
        for layer in [
            FWPM_LAYER_ALE_AUTH_CONNECT_V4,
            FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        ] {
            specifications.push(FilterSpec {
                layer,
                weight: PERMIT_WEIGHT,
                action: FWP_ACTION_PERMIT,
                conditions: vec![
                    ConditionSpec::Protocol(PROTOCOL_UDP),
                    ConditionSpec::Port(port),
                ],
            });
        }
    }

    for target in [&plan.accoding, &plan.server] {
        for address in &target.addresses {
            for protocol in [PROTOCOL_TCP, PROTOCOL_UDP] {
                specifications.push(permit_address(address, target.port, protocol));
            }
        }
    }

    for address in &plan.certificate {
        for port in CERTIFICATE_PORTS {
            specifications.push(permit_address(address, port, PROTOCOL_TCP));
        }
    }

    specifications
}

/// Builds a permit filter for one address and port.
fn permit_address(address: &IpAddr, port: u16, protocol: u8) -> FilterSpec {
    let conditions = vec![ConditionSpec::Protocol(protocol), ConditionSpec::Port(port)];
    match address {
        IpAddr::V4(v4) => permit_v4(v4.octets(), 32, conditions),
        IpAddr::V6(v6) => permit_v6(v6.octets(), 128, conditions),
    }
}

/// Builds a permit filter for an IPv4 prefix.
fn permit_v4(octets: [u8; 4], prefix: u8, mut conditions: Vec<ConditionSpec>) -> FilterSpec {
    conditions.insert(0, ConditionSpec::AddressV4(octets, prefix));
    FilterSpec {
        layer: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        weight: PERMIT_WEIGHT,
        action: FWP_ACTION_PERMIT,
        conditions,
    }
}

/// Builds a permit filter for an IPv6 prefix.
fn permit_v6(octets: [u8; 16], prefix: u8, mut conditions: Vec<ConditionSpec>) -> FilterSpec {
    conditions.insert(0, ConditionSpec::AddressV6(octets, prefix));
    FilterSpec {
        layer: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        weight: PERMIT_WEIGHT,
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
    use super::{BLOCK_WEIGHT, PERMIT_WEIGHT, filter_specs, prefix_mask_v4};
    use crate::egress::policy::{Plan, Target};
    use std::net::{IpAddr, Ipv4Addr};

    fn plan() -> Plan {
        Plan::new(
            Target::new(vec![IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10))], 443),
            Target::new(vec![IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)], 8443),
            vec![IpAddr::V4(Ipv4Addr::new(203, 0, 113, 20))],
            Vec::new(),
        )
    }

    #[test]
    fn a_block_filter_exists_for_both_families() {
        let specifications = filter_specs(&plan());
        let blocks = specifications
            .iter()
            .filter(|specification| specification.weight == BLOCK_WEIGHT)
            .count();
        assert_eq!(blocks, 2);
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
        assert!(BLOCK_WEIGHT < PERMIT_WEIGHT);
    }

    #[test]
    fn every_allowed_address_becomes_a_permit() {
        let specifications = filter_specs(&plan());
        // accoding (2 protocols) + server (2 protocols) + certificate (2 ports)
        let permits = specifications
            .iter()
            .filter(|specification| specification.weight == PERMIT_WEIGHT)
            .count();
        assert!(permits >= 6);
    }

    #[test]
    fn prefix_masks_cover_the_address() {
        assert_eq!(prefix_mask_v4(32), [255, 255, 255, 255]);
        assert_eq!(prefix_mask_v4(24), [255, 255, 255, 0]);
        assert_eq!(prefix_mask_v4(8), [255, 0, 0, 0]);
        assert_eq!(prefix_mask_v4(0), [0, 0, 0, 0]);
    }
}
