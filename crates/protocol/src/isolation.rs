//! Ports the **live** MiniDLNA occupies on this host.
//! rustyDLNA tests and the test container must never bind these.

/// MiniDLNA HTTP (`runtime_vars.port` default).
pub const LIVE_MINIDLNA_HTTP_PORT: u16 = 8200;
/// SSDP / multicast (`SSDP_PORT` in `minissdp.c`).
pub const LIVE_MINIDLNA_SSDP_PORT: u16 = 1900;

/// Isolated HTTP port for rustyDLNA tests and the test container.
pub const TEST_HTTP_PORT: u16 = 18200;
/// Isolated SSDP port for in-container listen tests (never published to the host).
pub const TEST_SSDP_PORT: u16 = 11900;

/// True if these listen ports would collide with the living MiniDLNA.
pub fn collides_with_live_minidlna(http_port: u16, ssdp_port: u16) -> bool {
    http_port == LIVE_MINIDLNA_HTTP_PORT || ssdp_port == LIVE_MINIDLNA_SSDP_PORT
}

/// Production may use 8200/1900 **after** MiniDLNA is stopped.
/// Tests must use the TEST_* ports.
pub fn test_listen_ports() -> (u16, u16) {
    (TEST_HTTP_PORT, TEST_SSDP_PORT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ports_do_not_collide() {
        let (http, ssdp) = test_listen_ports();
        assert_ne!(http, LIVE_MINIDLNA_HTTP_PORT);
        assert_ne!(ssdp, LIVE_MINIDLNA_SSDP_PORT);
        assert!(!collides_with_live_minidlna(http, ssdp));
    }

    #[test]
    fn live_ports_are_the_known_minidlna_ones() {
        assert_eq!(LIVE_MINIDLNA_HTTP_PORT, 8200);
        assert_eq!(LIVE_MINIDLNA_SSDP_PORT, 1900);
        assert!(collides_with_live_minidlna(8200, 11900));
        assert!(collides_with_live_minidlna(18200, 1900));
    }
}
