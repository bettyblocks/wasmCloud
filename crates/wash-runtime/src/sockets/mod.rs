use core::future::Future;
use core::ops::Deref;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use wasmtime::component::{HasData, ResourceTable};

pub(crate) mod host_instance_network;
pub(crate) mod host_ip_name_lookup;
pub(crate) mod host_network;
pub(crate) mod host_tcp;
pub(crate) mod host_tcp_create_socket;
pub(crate) mod host_udp;
pub(crate) mod host_udp_create_socket;
pub mod loopback;
pub(crate) mod network;
pub(crate) mod p2_tcp;
pub(crate) mod p2_udp;
pub(crate) mod tcp;
pub(crate) mod udp;
pub(crate) mod util;

pub(crate) mod host_ip_name_lookup_p3;
pub(crate) mod host_tcp_p3;
pub(crate) mod host_udp_p3;

pub use tcp::TcpSocket;
pub use udp::UdpSocket;

/// A helper struct which implements [`HasData`] for the `wasi:sockets` APIs.
pub struct WasiSockets;

impl HasData for WasiSockets {
    type Data<'a> = WasiSocketsCtxView<'a>;
}

/// Value taken from rust std library.
pub(crate) const DEFAULT_TCP_BACKLOG: u32 = 128;

/// Theoretical maximum byte size of a UDP datagram, the real limit is lower,
/// but we do not account for e.g. the transport layer here for simplicity.
/// In practice, datagrams are typically less than 1500 bytes.
pub(crate) const MAX_UDP_DATAGRAM_SIZE: usize = u16::MAX as usize;

/// [`crate::types::LocalResources`] `config` key opting a workload into DNS
/// resolution through `wasi:sockets/ip-name-lookup`.
///
/// Lookups are denied unless this is set (see [`AllowedNetworkUses::default`]),
/// so a component can only address a service by name when its workload asks for
/// it. Without the opt-in `resolve-addresses` answers
/// `permanent-resolver-failure`, and a component configured with a hostname
/// rather than a literal IP cannot reach its target at all.
pub const IP_NAME_LOOKUP_CONFIG_KEY: &str = "ip-name-lookup";

#[derive(Default)]
pub struct WasiSocketsCtx {
    pub(crate) socket_addr_check: SocketAddrCheck,
    pub(crate) allowed_network_uses: AllowedNetworkUses,
    pub(crate) loopback: Arc<std::sync::Mutex<loopback::Network>>,
}

pub struct WasiSocketsCtxView<'a> {
    pub ctx: &'a mut WasiSocketsCtx,
    pub table: &'a mut ResourceTable,
}

pub trait WasiSocketsView: Send {
    fn sockets(&mut self) -> WasiSocketsCtxView<'_>;
}

#[derive(Copy, Clone)]
pub(crate) struct AllowedNetworkUses {
    pub(crate) ip_name_lookup: bool,
    pub(crate) udp: bool,
    pub(crate) tcp: bool,
}

impl Default for AllowedNetworkUses {
    fn default() -> Self {
        Self {
            ip_name_lookup: false,
            udp: true,
            tcp: true,
        }
    }
}

impl AllowedNetworkUses {
    /// Network capabilities for a workload carrying this `LocalResources.config`.
    ///
    /// Only [`IP_NAME_LOOKUP_CONFIG_KEY`] is configurable. TCP and UDP keep their
    /// [`Default`] values because reachability is governed elsewhere — by the
    /// workload's [`SocketAddrCheck`] and its `allowed_hosts` allowlist — and
    /// resolving a name says nothing about being permitted to dial the result.
    ///
    /// The value is compared case-insensitively and trimmed. It reaches the
    /// runtime from hand-written YAML (a `wash dev` config, or a Kubernetes
    /// ConfigMap by way of the operator), where silently ignoring `True` or
    /// `"true "` costs more debugging than accepting either spelling. Anything
    /// that isn't `true` leaves lookups denied.
    pub(crate) fn from_component_config(config: &HashMap<String, String>) -> Self {
        Self {
            ip_name_lookup: config
                .get(IP_NAME_LOOKUP_CONFIG_KEY)
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("true")),
            ..Self::default()
        }
    }

    pub(crate) fn check_allowed_udp(self) -> std::io::Result<()> {
        if !self.udp {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "UDP is not allowed",
            ));
        }

        Ok(())
    }

    pub(crate) fn check_allowed_tcp(self) -> std::io::Result<()> {
        if !self.tcp {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "TCP is not allowed",
            ));
        }

        Ok(())
    }
}

type SocketAddrCheckFn = dyn Fn(SocketAddr, SocketAddrUse) -> Pin<Box<dyn Future<Output = bool> + Send + Sync>>
    + Send
    + Sync;

/// A check that will be called for each socket address that is used of whether the address is permitted.
#[derive(Clone)]
pub(crate) struct SocketAddrCheck(Arc<SocketAddrCheckFn>);

impl SocketAddrCheck {
    pub(crate) fn new(
        f: impl Fn(SocketAddr, SocketAddrUse) -> Pin<Box<dyn Future<Output = bool> + Send + Sync>>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self(Arc::new(f))
    }

    pub(crate) async fn check(
        &self,
        addr: SocketAddr,
        reason: SocketAddrUse,
    ) -> std::io::Result<()> {
        if (self.0)(addr, reason).await {
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "An address was not permitted by the socket address check.",
            ))
        }
    }
}

impl Deref for SocketAddrCheck {
    type Target = SocketAddrCheckFn;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}

impl Default for SocketAddrCheck {
    fn default() -> Self {
        Self(Arc::new(|_, _| Box::pin(async { false })))
    }
}

/// The reason what a socket address is being used for.
#[derive(Clone, Copy, Debug)]
pub enum SocketAddrUse {
    /// Binding TCP socket
    TcpBind,
    /// Connecting TCP socket
    TcpConnect,
    /// Binding UDP socket
    UdpBind,
    /// Connecting UDP socket
    UdpConnect,
    /// Sending datagram on non-connected UDP socket
    UdpOutgoingDatagram,
}

/// Convert our custom `util::ErrorCode` to the P3 bindings `ErrorCode`.
///
/// `util::ErrorCode` is a plain enum that carries no message payload, so
/// `Unknown` maps to `Other(None)` and there is nothing to forward. Callers that
/// have a meaningful message should construct `P3ErrorCode::Other(Some(..))`
/// directly rather than routing through this helper.
pub(crate) fn p3_error_code_from_util(
    error: util::ErrorCode,
) -> wasmtime_wasi::p3::bindings::sockets::types::ErrorCode {
    use wasmtime_wasi::p3::bindings::sockets::types::ErrorCode as P3ErrorCode;
    match error {
        util::ErrorCode::Unknown => P3ErrorCode::Other(None),
        util::ErrorCode::AccessDenied => P3ErrorCode::AccessDenied,
        util::ErrorCode::NotSupported => P3ErrorCode::NotSupported,
        util::ErrorCode::InvalidArgument => P3ErrorCode::InvalidArgument,
        util::ErrorCode::OutOfMemory => P3ErrorCode::OutOfMemory,
        util::ErrorCode::Timeout => P3ErrorCode::Timeout,
        util::ErrorCode::InvalidState => P3ErrorCode::InvalidState,
        util::ErrorCode::AddressNotBindable => P3ErrorCode::AddressNotBindable,
        util::ErrorCode::AddressInUse => P3ErrorCode::AddressInUse,
        util::ErrorCode::RemoteUnreachable => P3ErrorCode::RemoteUnreachable,
        util::ErrorCode::ConnectionRefused => P3ErrorCode::ConnectionRefused,
        util::ErrorCode::ConnectionReset => P3ErrorCode::ConnectionReset,
        util::ErrorCode::ConnectionAborted => P3ErrorCode::ConnectionAborted,
        util::ErrorCode::DatagramTooLarge => P3ErrorCode::DatagramTooLarge,
        util::ErrorCode::NotInProgress => P3ErrorCode::InvalidState,
        util::ErrorCode::ConcurrencyConflict => P3ErrorCode::InvalidState,
    }
}

/// Convert our `util::ErrorCode` to a P3 `SocketError` (TrappableError).
pub(crate) fn p3_socket_error_from_util(
    error: util::ErrorCode,
) -> wasmtime_wasi::p3::sockets::SocketError {
    p3_error_code_from_util(error).into()
}

/// Register P3 socket interfaces with the linker using our custom socket implementation.
pub fn add_p3_to_linker(
    linker: &mut wasmtime::component::Linker<crate::engine::ctx::SharedCtx>,
) -> anyhow::Result<()> {
    use wasmtime_wasi::p3::bindings::sockets::{ip_name_lookup, types};
    ip_name_lookup::add_to_linker::<_, WasiSockets>(linker, crate::engine::ctx::extract_sockets)?;
    types::add_to_linker::<_, WasiSockets>(linker, crate::engine::ctx::extract_sockets)?;
    Ok(())
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum SocketAddressFamily {
    Ipv4,
    Ipv6,
}

impl From<SocketAddressFamily> for wasmtime_wasi::p3::bindings::sockets::types::IpAddressFamily {
    fn from(family: SocketAddressFamily) -> Self {
        match family {
            SocketAddressFamily::Ipv4 => Self::Ipv4,
            SocketAddressFamily::Ipv6 => Self::Ipv6,
        }
    }
}

#[cfg(test)]
mod tests_allowed_network_uses {
    use super::*;

    /// `AllowedNetworkUses` is `pub(crate)`, so these exercise it in place rather
    /// than through an integration test.
    fn config(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// Absent key keeps the deny-by-default posture: a workload that never asked
    /// for name resolution must not receive it.
    #[test]
    fn absent_key_denies_name_lookup() {
        assert!(!AllowedNetworkUses::from_component_config(&config(&[])).ip_name_lookup);
        assert!(
            !AllowedNetworkUses::from_component_config(&config(&[("tracing", "disable")]))
                .ip_name_lookup
        );
    }

    #[test]
    fn true_allows_name_lookup() {
        assert!(
            AllowedNetworkUses::from_component_config(&config(&[(
                IP_NAME_LOOKUP_CONFIG_KEY,
                "true"
            )]))
            .ip_name_lookup
        );
    }

    /// The value comes from hand-written YAML, so casing and stray whitespace are
    /// accepted rather than silently turning the capability off.
    #[test]
    fn true_is_matched_leniently() {
        for value in ["True", "TRUE", " true", "true "] {
            assert!(
                AllowedNetworkUses::from_component_config(&config(&[(
                    IP_NAME_LOOKUP_CONFIG_KEY,
                    value
                )]))
                .ip_name_lookup,
                "{value:?} should enable name lookup"
            );
        }
    }

    /// Anything that isn't `true` denies, so a typo fails closed.
    #[test]
    fn other_values_deny_name_lookup() {
        for value in ["false", "", "1", "yes", "enable", "ture"] {
            assert!(
                !AllowedNetworkUses::from_component_config(&config(&[(
                    IP_NAME_LOOKUP_CONFIG_KEY,
                    value
                )]))
                .ip_name_lookup,
                "{value:?} should not enable name lookup"
            );
        }
    }

    /// TCP and UDP are not configurable here — reachability stays with the
    /// socket-address check and `allowed_hosts`.
    #[test]
    fn tcp_and_udp_keep_their_defaults() {
        let uses = AllowedNetworkUses::from_component_config(&config(&[(
            IP_NAME_LOOKUP_CONFIG_KEY,
            "true",
        )]));
        assert!(uses.check_allowed_tcp().is_ok());
        assert!(uses.check_allowed_udp().is_ok());
    }
}

#[cfg(test)]
mod tests_p3 {
    use super::*;

    #[test]
    fn test_p3_error_code_maps_all_variants() {
        use wasmtime_wasi::p3::bindings::sockets::types::ErrorCode as P3;

        assert!(matches!(
            p3_error_code_from_util(util::ErrorCode::AccessDenied),
            P3::AccessDenied
        ));
        assert!(matches!(
            p3_error_code_from_util(util::ErrorCode::NotSupported),
            P3::NotSupported
        ));
        assert!(matches!(
            p3_error_code_from_util(util::ErrorCode::InvalidArgument),
            P3::InvalidArgument
        ));
        assert!(matches!(
            p3_error_code_from_util(util::ErrorCode::OutOfMemory),
            P3::OutOfMemory
        ));
        assert!(matches!(
            p3_error_code_from_util(util::ErrorCode::Timeout),
            P3::Timeout
        ));
        assert!(matches!(
            p3_error_code_from_util(util::ErrorCode::InvalidState),
            P3::InvalidState
        ));
        assert!(matches!(
            p3_error_code_from_util(util::ErrorCode::AddressNotBindable),
            P3::AddressNotBindable
        ));
        assert!(matches!(
            p3_error_code_from_util(util::ErrorCode::AddressInUse),
            P3::AddressInUse
        ));
        assert!(matches!(
            p3_error_code_from_util(util::ErrorCode::RemoteUnreachable),
            P3::RemoteUnreachable
        ));
        assert!(matches!(
            p3_error_code_from_util(util::ErrorCode::ConnectionRefused),
            P3::ConnectionRefused
        ));
        assert!(matches!(
            p3_error_code_from_util(util::ErrorCode::ConnectionReset),
            P3::ConnectionReset
        ));
        assert!(matches!(
            p3_error_code_from_util(util::ErrorCode::ConnectionAborted),
            P3::ConnectionAborted
        ));
        assert!(matches!(
            p3_error_code_from_util(util::ErrorCode::DatagramTooLarge),
            P3::DatagramTooLarge
        ));
    }

    #[test]
    fn test_p3_error_code_maps_p2_only_variants_to_invalidstate() {
        use wasmtime_wasi::p3::bindings::sockets::types::ErrorCode as P3;

        // P3 collapsed NotInProgress and ConcurrencyConflict into InvalidState
        assert!(matches!(
            p3_error_code_from_util(util::ErrorCode::NotInProgress),
            P3::InvalidState
        ));
        assert!(matches!(
            p3_error_code_from_util(util::ErrorCode::ConcurrencyConflict),
            P3::InvalidState
        ));
    }

    #[test]
    fn test_p3_error_code_maps_unknown_to_other() {
        use wasmtime_wasi::p3::bindings::sockets::types::ErrorCode as P3;

        assert!(matches!(
            p3_error_code_from_util(util::ErrorCode::Unknown),
            P3::Other(None)
        ));
    }

    #[test]
    fn test_p3_socket_error_from_util_converts() {
        // Just verify it doesn't panic and produces a SocketError
        let err = p3_socket_error_from_util(util::ErrorCode::ConnectionRefused);
        let code = err.downcast().expect("should downcast to ErrorCode");
        assert!(matches!(
            code,
            wasmtime_wasi::p3::bindings::sockets::types::ErrorCode::ConnectionRefused
        ));
    }
}
