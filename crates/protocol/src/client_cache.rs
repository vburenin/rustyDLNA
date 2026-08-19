//! 25-slot IPv4 client cache matching MiniDLNA's SearchClientCache/AddClientCache behavior.

use std::net::Ipv4Addr;

use crate::clients::{ClientKind, ClientProfile, CLIENTS};

pub const CLIENT_CACHE_SLOTS: usize = 25;
pub const CLIENT_CACHE_TTL_SECS: u64 = 3600;

#[derive(Clone, Copy, Debug)]
pub struct ClientCacheEntry {
    pub addr: Ipv4Addr,
    pub mac: Option<[u8; 6]>,
    pub profile: &'static ClientProfile,
    pub age: u64,
}

#[derive(Clone, Debug)]
pub struct ClientCache {
    slots: [Option<ClientCacheEntry>; CLIENT_CACHE_SLOTS],
}

impl Default for ClientCache {
    fn default() -> Self {
        Self {
            slots: [None; CLIENT_CACHE_SLOTS],
        }
    }
}

impl ClientCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    pub fn set_age(&mut self, addr: Ipv4Addr, age: u64) {
        for ent in self.slots.iter_mut().flatten() {
            if ent.addr == addr {
                ent.age = age;
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Search by IPv4. Age > 3600s expires unless the same MAC extends another hour.
    pub fn search(
        &mut self,
        addr: Ipv4Addr,
        now: u64,
        mac: Option<[u8; 6]>,
    ) -> Option<&'static ClientProfile> {
        for slot in &mut self.slots {
            let Some(ent) = slot else {
                continue;
            };
            if ent.addr != addr {
                continue;
            }
            if now.saturating_sub(ent.age) > CLIENT_CACHE_TTL_SECS {
                let same_mac = match (mac, ent.mac) {
                    (Some(a), Some(b)) => a == b,
                    _ => false,
                };
                if same_mac {
                    ent.age = now;
                } else {
                    *slot = None;
                    return None;
                }
            }
            return Some(ent.profile);
        }
        None
    }

    pub fn add(
        &mut self,
        addr: Ipv4Addr,
        profile: &'static ClientProfile,
        mac: Option<[u8; 6]>,
        now: u64,
    ) -> bool {
        if let Some(ent) = self.slots.iter_mut().flatten().find(|e| e.addr == addr) {
            if should_overwrite(ent.profile, profile) {
                ent.profile = profile;
                ent.mac = mac.or(ent.mac);
                ent.age = now;
            }
            return true;
        }
        if let Some(slot) = self.slots.iter_mut().find(|s| s.is_none()) {
            *slot = Some(ClientCacheEntry {
                addr,
                mac,
                profile,
                age: now,
            });
            return true;
        }
        false
    }

    /// Identify this request, then cache: specific UA wins; generic must not
    /// clobber a more specific cached type (`type < StandardDlna150`).
    pub fn remember(
        &mut self,
        addr: Ipv4Addr,
        request: &'static ClientProfile,
        mac: Option<[u8; 6]>,
        now: u64,
    ) -> &'static ClientProfile {
        if is_generic(request) {
            if let Some(cached) = self.search(addr, now, mac) {
                if !is_generic(cached) {
                    return cached;
                }
            }
        }
        let _ = self.add(addr, request, mac, now);
        if let Some(cached) = self.search(addr, now, mac) {
            cached
        } else {
            request
        }
    }
}

fn is_generic(p: &ClientProfile) -> bool {
    matches!(
        p.kind,
        ClientKind::StandardDlna150 | ClientKind::StandardUpnp | ClientKind::Unknown
    )
}

/// Do not overwrite `type < StandardDlna150` with generic DLNADOC/1.50 / UPnP/1.0.
/// Samsung Series B is not overwritten by Series A.
fn should_overwrite(old: &ClientProfile, new: &ClientProfile) -> bool {
    if old.kind == ClientKind::SamsungSeriesB && new.kind == ClientKind::SamsungSeriesA {
        return false;
    }
    if is_generic(new) && !is_generic(old) {
        return false;
    }
    if (old.kind as u8) < (ClientKind::StandardDlna150 as u8) && is_generic(new) {
        return false;
    }
    true
}

pub fn generic_dlna150() -> &'static ClientProfile {
    CLIENTS
        .iter()
        .find(|c| c.kind == ClientKind::StandardDlna150)
        .expect("DLNADOC/1.50 row")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identify_user_agent;

    #[test]
    fn cache_keeps_kodi_when_generic_ua_follows() {
        let kodi = identify_user_agent("Kodi/21.0").expect("kodi");
        let generic = identify_user_agent("DLNADOC/1.50").expect("generic");
        assert_eq!(generic.kind, ClientKind::StandardDlna150);
        let ip: Ipv4Addr = "192.0.2.10".parse().unwrap();
        let mut cache = ClientCache::new();
        let first = cache.remember(ip, kodi, None, 1_000);
        assert_eq!(first.kind, ClientKind::Kodi);
        let second = cache.remember(ip, generic, None, 1_010);
        assert_eq!(
            second.kind,
            ClientKind::Kodi,
            "generic UA must not clobber Kodi"
        );
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn cache_expires_after_one_hour() {
        let kodi = identify_user_agent("Kodi/21.0").expect("kodi");
        let generic = identify_user_agent("DLNADOC/1.50").expect("generic");
        let ip: Ipv4Addr = "192.0.2.11".parse().unwrap();
        let mac = Some([1, 2, 3, 4, 5, 6]);
        let mut cache = ClientCache::new();
        cache.remember(ip, kodi, mac, 0);
        assert_eq!(
            cache
                .search(ip, CLIENT_CACHE_TTL_SECS + 1, mac)
                .map(|p| p.kind),
            Some(ClientKind::Kodi),
            "same MAC extends another hour"
        );
        let mut cache2 = ClientCache::new();
        cache2.remember(ip, kodi, mac, 0);
        assert!(
            cache2.search(ip, CLIENT_CACHE_TTL_SECS + 1, None).is_none(),
            "expired without matching MAC"
        );
        let after = cache2.remember(ip, generic, None, CLIENT_CACHE_TTL_SECS + 2);
        assert_eq!(after.kind, ClientKind::StandardDlna150);
    }

    #[test]
    fn samsung_b_not_overwritten_by_a() {
        let b = CLIENTS
            .iter()
            .find(|c| c.kind == ClientKind::SamsungSeriesB)
            .unwrap();
        let a = CLIENTS
            .iter()
            .find(|c| c.kind == ClientKind::SamsungSeriesA)
            .unwrap();
        let ip: Ipv4Addr = "192.0.2.12".parse().unwrap();
        let mut cache = ClientCache::new();
        cache.remember(ip, b, None, 1);
        let _ = cache.add(ip, a, None, 2);
        assert_eq!(
            cache.search(ip, 3, None).unwrap().kind,
            ClientKind::SamsungSeriesB
        );
    }
}
