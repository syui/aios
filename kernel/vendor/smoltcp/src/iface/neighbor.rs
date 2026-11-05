// Heads up! Before working on this file you should read, at least,
// the parts of RFC 1122 that discuss ARP.

use heapless::LinearMap;

use crate::config::IFACE_NEIGHBOR_CACHE_COUNT;
use crate::time::{Duration, Instant};
use crate::wire::{HardwareAddress, IpAddress};

/// A cached neighbor.
///
/// A neighbor mapping translates from a protocol address to a hardware address,
/// and contains the timestamp past which the mapping should be discarded.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct Neighbor {
    hardware_addr: HardwareAddress,
    expires_at: Instant,
}

/// An answer to a neighbor cache lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub(crate) enum Answer {
    /// The neighbor address is in the cache and not expired.
    Found(HardwareAddress),
    /// The neighbor address is not in the cache, or has expired.
    NotFound,
    /// The neighbor address is not in the cache, or has expired,
    /// and a lookup has been made recently.
    RateLimited,
    /// aios: `MAX_PROBES` discovery requests got no answer (Linux's NUD_FAILED): drop the packet.
    Failed,
}

/// aios: discovery state of one address: no new request until `until`, and how many were sent in a row.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
struct Probe {
    until: Instant,
    probes: u8,
}

impl Answer {
    /// Returns whether a valid address was found.
    pub(crate) fn found(&self) -> bool {
        match self {
            Answer::Found(_) => true,
            _ => false,
        }
    }
}

/// A neighbor cache backed by a map.
#[derive(Debug)]
pub struct Cache {
    storage: LinearMap<IpAddress, Neighbor, IFACE_NEIGHBOR_CACHE_COUNT>,
    /// aios: per-address time until which no new discovery request is sent (Linux's per-neighbour retrans_time),
    /// and the number of requests sent in a row. Upstream keeps a single `silent_until` for the whole cache, so
    /// one request silences every other address for a second, and a socket that keeps retrying one unreachable
    /// address starves all the others.
    silent_until: LinearMap<IpAddress, Probe, IFACE_NEIGHBOR_CACHE_COUNT>,
}

impl Cache {
    /// Minimum delay between discovery requests, in milliseconds.
    pub(crate) const SILENT_TIME: Duration = Duration::from_millis(1_000);

    /// Neighbor entry lifetime, in milliseconds.
    pub(crate) const ENTRY_LIFETIME: Duration = Duration::from_millis(60_000);

    /// aios: discovery requests without an answer before the address fails (Linux's mcast_probes, 3).
    pub(crate) const MAX_PROBES: u8 = 3;

    /// Create a cache.
    pub fn new() -> Self {
        Self {
            storage: LinearMap::new(),
            silent_until: LinearMap::new(),
        }
    }

    pub fn reset_expiry_if_existing(
        &mut self,
        protocol_addr: IpAddress,
        source_hardware_addr: HardwareAddress,
        timestamp: Instant,
    ) {
        if let Some(Neighbor {
            expires_at,
            hardware_addr,
        }) = self.storage.get_mut(&protocol_addr)
        {
            if source_hardware_addr == *hardware_addr {
                *expires_at = timestamp + Self::ENTRY_LIFETIME;
            }
        }
    }

    pub fn fill(
        &mut self,
        protocol_addr: IpAddress,
        hardware_addr: HardwareAddress,
        timestamp: Instant,
    ) {
        debug_assert!(protocol_addr.is_unicast());
        debug_assert!(hardware_addr.is_unicast());

        let expires_at = timestamp + Self::ENTRY_LIFETIME;
        self.fill_with_expiration(protocol_addr, hardware_addr, expires_at);
    }

    pub fn fill_with_expiration(
        &mut self,
        protocol_addr: IpAddress,
        hardware_addr: HardwareAddress,
        expires_at: Instant,
    ) {
        debug_assert!(protocol_addr.is_unicast());
        debug_assert!(hardware_addr.is_unicast());

        let neighbor = Neighbor {
            expires_at,
            hardware_addr,
        };
        // aios: an answer ends the discovery
        self.silent_until.remove(&protocol_addr);
        match self.storage.insert(protocol_addr, neighbor) {
            Ok(Some(old_neighbor)) => {
                if old_neighbor.hardware_addr != hardware_addr {
                    net_trace!(
                        "replaced {} => {} (was {})",
                        protocol_addr,
                        hardware_addr,
                        old_neighbor.hardware_addr
                    );
                }
            }
            Ok(None) => {
                net_trace!("filled {} => {} (was empty)", protocol_addr, hardware_addr);
            }
            Err((protocol_addr, neighbor)) => {
                // If we're going down this branch, it means the cache is full, and we need to evict an entry.
                let old_protocol_addr = *self
                    .storage
                    .iter()
                    .min_by_key(|(_, neighbor)| neighbor.expires_at)
                    .expect("empty neighbor cache storage")
                    .0;

                let _old_neighbor = self.storage.remove(&old_protocol_addr).unwrap();
                match self.storage.insert(protocol_addr, neighbor) {
                    Ok(None) => {
                        net_trace!(
                            "filled {} => {} (evicted {} => {})",
                            protocol_addr,
                            hardware_addr,
                            old_protocol_addr,
                            _old_neighbor.hardware_addr
                        );
                    }
                    // We've covered everything else above.
                    _ => unreachable!(),
                }
            }
        }
    }

    pub(crate) fn lookup(&self, protocol_addr: &IpAddress, timestamp: Instant) -> Answer {
        assert!(protocol_addr.is_unicast());

        if let Some(&Neighbor {
            expires_at,
            hardware_addr,
        }) = self.storage.get(protocol_addr)
        {
            if timestamp < expires_at {
                return Answer::Found(hardware_addr);
            }
        }

        match self.silent_until.get(protocol_addr) {
            Some(p) if timestamp < p.until => Answer::RateLimited,
            // aios: the last of MAX_PROBES requests went unanswered (and the retry came in time: a request long
            // ago starts a new discovery)
            Some(p) if p.probes >= Self::MAX_PROBES && timestamp < p.until + Self::SILENT_TIME => Answer::Failed,
            _ => Answer::NotFound,
        }
    }

    /// aios: a discovery request for `protocol_addr` was sent; silence only that address, and count it (a request
    /// more than SILENT_TIME after the previous silence ended starts the count again).
    pub(crate) fn limit_rate(&mut self, protocol_addr: IpAddress, timestamp: Instant) {
        let until = timestamp + Self::SILENT_TIME;
        if let Some(p) = self.silent_until.get_mut(&protocol_addr) {
            let fresh = timestamp >= p.until + Self::SILENT_TIME;
            p.probes = if fresh { 1 } else { p.probes.saturating_add(1) };
            p.until = until;
            return;
        }
        let probe = Probe { until, probes: 1 };
        if let Err((addr, probe)) = self.silent_until.insert(protocol_addr, probe) {
            // Full: forget the address whose silence ends first (usually one already past).
            let oldest = *self
                .silent_until
                .iter()
                .min_by_key(|(_, p)| p.until)
                .expect("empty silence map")
                .0;
            self.silent_until.remove(&oldest);
            let _ = self.silent_until.insert(addr, probe);
        }
    }

    /// aios: the discovery of `protocol_addr` failed and was reported; the next packet starts a new one.
    pub(crate) fn forget_failed(&mut self, protocol_addr: &IpAddress) {
        self.silent_until.remove(protocol_addr);
    }

    pub(crate) fn flush(&mut self) {
        self.storage.clear()
    }
}

#[cfg(feature = "medium-ethernet")]
#[cfg(test)]
mod test {
    use super::*;
    #[cfg(all(feature = "proto-ipv4", not(feature = "proto-ipv6")))]
    use crate::wire::ipv4::test::{MOCK_IP_ADDR_1, MOCK_IP_ADDR_2, MOCK_IP_ADDR_3, MOCK_IP_ADDR_4};
    #[cfg(feature = "proto-ipv6")]
    use crate::wire::ipv6::test::{MOCK_IP_ADDR_1, MOCK_IP_ADDR_2, MOCK_IP_ADDR_3, MOCK_IP_ADDR_4};

    use crate::wire::EthernetAddress;

    const HADDR_A: HardwareAddress = HardwareAddress::Ethernet(EthernetAddress([0, 0, 0, 0, 0, 1]));
    const HADDR_B: HardwareAddress = HardwareAddress::Ethernet(EthernetAddress([0, 0, 0, 0, 0, 2]));
    const HADDR_C: HardwareAddress = HardwareAddress::Ethernet(EthernetAddress([0, 0, 0, 0, 0, 3]));
    const HADDR_D: HardwareAddress = HardwareAddress::Ethernet(EthernetAddress([0, 0, 0, 0, 0, 4]));

    #[test]
    fn test_fill() {
        let mut cache = Cache::new();

        assert!(!cache
            .lookup(&MOCK_IP_ADDR_1.into(), Instant::from_millis(0))
            .found());
        assert!(!cache
            .lookup(&MOCK_IP_ADDR_2.into(), Instant::from_millis(0))
            .found());

        cache.fill(MOCK_IP_ADDR_1.into(), HADDR_A, Instant::from_millis(0));
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1.into(), Instant::from_millis(0)),
            Answer::Found(HADDR_A)
        );
        assert!(!cache
            .lookup(&MOCK_IP_ADDR_2.into(), Instant::from_millis(0))
            .found());
        assert!(!cache
            .lookup(
                &MOCK_IP_ADDR_1.into(),
                Instant::from_millis(0) + Cache::ENTRY_LIFETIME * 2
            )
            .found(),);

        cache.fill(MOCK_IP_ADDR_1.into(), HADDR_A, Instant::from_millis(0));
        assert!(!cache
            .lookup(&MOCK_IP_ADDR_2.into(), Instant::from_millis(0))
            .found());
    }

    #[test]
    fn test_expire() {
        let mut cache = Cache::new();

        cache.fill(MOCK_IP_ADDR_1.into(), HADDR_A, Instant::from_millis(0));
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1.into(), Instant::from_millis(0)),
            Answer::Found(HADDR_A)
        );
        assert!(!cache
            .lookup(
                &MOCK_IP_ADDR_1.into(),
                Instant::from_millis(0) + Cache::ENTRY_LIFETIME * 2
            )
            .found(),);
    }

    #[test]
    fn test_replace() {
        let mut cache = Cache::new();

        cache.fill(MOCK_IP_ADDR_1.into(), HADDR_A, Instant::from_millis(0));
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1.into(), Instant::from_millis(0)),
            Answer::Found(HADDR_A)
        );
        cache.fill(MOCK_IP_ADDR_1.into(), HADDR_B, Instant::from_millis(0));
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1.into(), Instant::from_millis(0)),
            Answer::Found(HADDR_B)
        );
    }

    #[test]
    fn test_evict() {
        let mut cache = Cache::new();

        cache.fill(MOCK_IP_ADDR_1.into(), HADDR_A, Instant::from_millis(100));
        cache.fill(MOCK_IP_ADDR_2.into(), HADDR_B, Instant::from_millis(50));
        cache.fill(MOCK_IP_ADDR_3.into(), HADDR_C, Instant::from_millis(200));
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_2.into(), Instant::from_millis(1000)),
            Answer::Found(HADDR_B)
        );
        assert!(!cache
            .lookup(&MOCK_IP_ADDR_4.into(), Instant::from_millis(1000))
            .found());

        cache.fill(MOCK_IP_ADDR_4.into(), HADDR_D, Instant::from_millis(300));
        assert!(!cache
            .lookup(&MOCK_IP_ADDR_2.into(), Instant::from_millis(1000))
            .found());
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_4.into(), Instant::from_millis(1000)),
            Answer::Found(HADDR_D)
        );
    }

    #[test]
    fn test_hush() {
        let mut cache = Cache::new();

        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1.into(), Instant::from_millis(0)),
            Answer::NotFound
        );

        cache.limit_rate(MOCK_IP_ADDR_1.into(), Instant::from_millis(0));
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1.into(), Instant::from_millis(100)),
            Answer::RateLimited
        );
        // aios: other addresses are not silenced
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_2.into(), Instant::from_millis(100)),
            Answer::NotFound
        );
        // aios: three unanswered requests in a row fail
        cache.limit_rate(MOCK_IP_ADDR_1.into(), Instant::from_millis(1000));
        cache.limit_rate(MOCK_IP_ADDR_1.into(), Instant::from_millis(2000));
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1.into(), Instant::from_millis(3000)),
            Answer::Failed
        );
        cache.forget_failed(&MOCK_IP_ADDR_1.into());
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1.into(), Instant::from_millis(3000)),
            Answer::NotFound
        );
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1.into(), Instant::from_millis(2000)),
            Answer::NotFound
        );
    }

    #[test]
    fn test_flush() {
        let mut cache = Cache::new();

        cache.fill(MOCK_IP_ADDR_1.into(), HADDR_A, Instant::from_millis(0));
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1.into(), Instant::from_millis(0)),
            Answer::Found(HADDR_A)
        );
        assert!(!cache
            .lookup(&MOCK_IP_ADDR_2.into(), Instant::from_millis(0))
            .found());

        cache.flush();
        assert!(!cache
            .lookup(&MOCK_IP_ADDR_1.into(), Instant::from_millis(0))
            .found());
        assert!(!cache
            .lookup(&MOCK_IP_ADDR_1.into(), Instant::from_millis(0))
            .found());
    }
}
