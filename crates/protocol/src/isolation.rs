//! Ports the **live** daemon occupies. Tests and the test container
//! must never bind these.

/// Live HTTP (default `runtime_vars.port`).
pub const LIVE_HTTP_PORT: u16 = 8200;
/// SSDP / multicast.
pub const LIVE_SSDP_PORT: u16 = 1900;

/// Isolated HTTP port for rustyDLNA tests and the test container.
pub const TEST_HTTP_PORT: u16 = 18200;
/// Isolated SSDP port for in-container listen tests (never published to the host).
pub const TEST_SSDP_PORT: u16 = 11900;

/// True if these listen ports would collide with the live daemon.
pub fn collides_with_live_ports(http_port: u16, ssdp_port: u16) -> bool {
    http_port == LIVE_HTTP_PORT || ssdp_port == LIVE_SSDP_PORT
}

/// Production may use 8200/1900. Tests must use the TEST_* ports.
pub fn test_listen_ports() -> (u16, u16) {
    (TEST_HTTP_PORT, TEST_SSDP_PORT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ports_do_not_collide() {
        let (http, ssdp) = test_listen_ports();
        assert_ne!(http, LIVE_HTTP_PORT);
        assert_ne!(ssdp, LIVE_SSDP_PORT);
        assert!(!collides_with_live_ports(http, ssdp));
    }

    #[test]
    fn live_ports_are_8200_and_1900() {
        assert_eq!(LIVE_HTTP_PORT, 8200);
        assert_eq!(LIVE_SSDP_PORT, 1900);
        assert!(collides_with_live_ports(8200, 11900));
        assert!(collides_with_live_ports(18200, 1900));
    }
}
