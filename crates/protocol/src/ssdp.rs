//! SSDP literals from `src/minissdp.c`.

pub const SSDP_PORT: u16 = 1900;
pub const SSDP_MCAST_ADDR: &str = "239.255.255.250";
pub const SSDP_NOTIFY_TTL: u8 = 4;
pub const MAN_DISCOVER: &str = "\"ssdp:discover\"";
pub const NTS_ALIVE: &str = "ssdp:alive";
pub const NTS_BYEBYE: &str = "ssdp:byebye";
pub const ST_ALL: &str = "ssdp:all";
pub const NT_ROOTDEVICE: &str = "upnp:rootdevice";
pub const ST_MEDIASERVER: &str = "urn:schemas-upnp-org:device:MediaServer:1";
pub const ST_CONTENTDIRECTORY: &str = "urn:schemas-upnp-org:service:ContentDirectory:1";
pub const ST_CONNECTIONMANAGER: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";
pub const ST_MS_REGISTRAR: &str = "urn:microsoft.com:service:X_MS_MediaReceiverRegistrar:1";

/// `CACHE-CONTROL` / notify lifetime: `(notify_interval << 1) + 10`.
pub fn notify_max_age(notify_interval_secs: u32) -> u32 {
    notify_interval_secs.saturating_mul(2).saturating_add(10)
}

pub fn known_service_types<'a>(uuid: &'a str) -> [&'a str; 6] {
    [
        uuid,
        NT_ROOTDEVICE,
        ST_MEDIASERVER,
        ST_CONTENTDIRECTORY,
        ST_CONNECTIONMANAGER,
        ST_MS_REGISTRAR,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multicast_and_lifetime() {
        assert_eq!(SSDP_MCAST_ADDR, "239.255.255.250");
        assert_eq!(SSDP_PORT, 1900);
        assert_eq!(notify_max_age(895), 1800);
        assert_eq!(NTS_ALIVE, "ssdp:alive");
    }
}
