//! LAN discovery via UDP broadcast.
//!
//! Hosts publish a periodic `LanAdvert` to UDP `255.255.255.255:LAN_PORT`
//! describing the game; clients listen on the same port and surface live
//! adverts in the lobby browser. Adverts older than `LISTEN_TTL` are
//! evicted.
//!
//! v1: unicast IPv4 broadcast only — no multicast (firewall friction on
//! Windows/macOS). Same port for advertise + listen so a single-machine
//! LAN test (host + client on `127.0.0.1`) sees the loopback advert.
//!
//! Wire shape (`LanAdvert`) is bincode-serialised; payload stays well
//! under 512 bytes so it fits a single non-fragmented UDP packet.

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ahash::AHashMap;
use bevy::prelude::*;
use serde::{Deserialize, Serialize};

/// Discovery channel — distinct from the game socket so a single host
/// can advertise on the LAN port while the game runs on its own
/// `--bind` port.
pub const LAN_PORT: u16 = 5001;

/// Listener evicts adverts older than this. ~3× the broadcast interval
/// so a transient packet loss doesn't make the host disappear.
pub const LISTEN_TTL: Duration = Duration::from_secs(3);

/// Broadcast period.
pub const BROADCAST_INTERVAL: Duration = Duration::from_secs(1);

/// Coarse lobby phase wire-projection. Mirrors `LobbyPhase` but lives
/// in this module so the LAN browser can render without depending on
/// the server-side lobby_state module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdvertPhase {
    Lobby,
    InGame,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanAdvert {
    pub protocol_version: u32,
    pub game_name: String,
    pub host_name: String,
    /// `game_port` (NOT `LAN_PORT`) — the port the client should
    /// `--connect` to. Sender's IP comes from `recv_from` on the
    /// listener side; only the port is in-payload.
    pub game_port: u16,
    pub players: u8,
    pub max_players: u8,
    pub phase: AdvertPhase,
    pub world_seed: u64,
}

/// Browser-visible entry seen by the listener.
#[derive(Debug, Clone)]
pub struct DiscoveredHost {
    pub advert: LanAdvert,
    pub host_addr: SocketAddrV4,
    pub last_seen: Instant,
}

/// Bevy Resource holding the listener's live host map. Cloned (cheap —
/// `Arc<Mutex>`) into both the listener thread and the lobby UI system.
#[derive(Resource, Clone, Default)]
pub struct LanBrowser {
    inner: Arc<Mutex<LanBrowserInner>>,
}

#[derive(Default)]
struct LanBrowserInner {
    hosts: AHashMap<SocketAddrV4, DiscoveredHost>,
}

impl LanBrowser {
    pub fn fresh(&self) -> Vec<DiscoveredHost> {
        let mut guard = self.inner.lock().expect("LanBrowser mutex poisoned");
        let now = Instant::now();
        guard
            .hosts
            .retain(|_, host| now.duration_since(host.last_seen) <= LISTEN_TTL);
        let mut out: Vec<DiscoveredHost> = guard.hosts.values().cloned().collect();
        out.sort_by_key(|h| h.host_addr);
        out
    }
}

/// Spawn the listener background thread + return its `LanBrowser`
/// handle. Bind failure (e.g. port in use after a sibling process
/// dropped) → one retry after 500 ms; second failure logs + returns
/// an empty browser (the lobby UI's manual-address field is the
/// fallback path).
pub fn spawn_listener_thread() -> LanBrowser {
    let browser = LanBrowser::default();
    let bound = bind_listener().or_else(|err| {
        warn!("LAN listener bind failed ({err}); retrying once after 500ms");
        std::thread::sleep(Duration::from_millis(500));
        bind_listener()
    });
    let socket = match bound {
        Ok(s) => s,
        Err(err) => {
            warn!("LAN listener bind failed twice ({err}); browser disabled");
            return browser;
        }
    };
    let browser_for_thread = browser.clone();
    std::thread::spawn(move || run_listener(socket, browser_for_thread));
    browser
}

fn bind_listener() -> std::io::Result<UdpSocket> {
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, LAN_PORT))?;
    socket.set_read_timeout(Some(Duration::from_millis(500)))?;
    socket.set_broadcast(true)?;
    Ok(socket)
}

fn run_listener(socket: UdpSocket, browser: LanBrowser) {
    let mut buf = [0u8; 1024];
    loop {
        match socket.recv_from(&mut buf) {
            Ok((n, peer)) => {
                let std::net::SocketAddr::V4(peer_v4) = peer else {
                    continue;
                };
                if let Ok(advert) = bincode::deserialize::<LanAdvert>(&buf[..n]) {
                    if let Ok(mut guard) = browser.inner.lock() {
                        guard.hosts.insert(
                            peer_v4,
                            DiscoveredHost {
                                advert,
                                host_addr: peer_v4,
                                last_seen: Instant::now(),
                            },
                        );
                    }
                }
            }
            Err(err) => match err.kind() {
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                    // Periodic GC tick — touch the map to evict stale entries
                    // even when no fresh adverts arrive.
                    if let Ok(mut guard) = browser.inner.lock() {
                        let now = Instant::now();
                        guard
                            .hosts
                            .retain(|_, h| now.duration_since(h.last_seen) <= LISTEN_TTL);
                    }
                }
                _ => {
                    warn!("LAN listener recv error: {err}");
                    std::thread::sleep(Duration::from_millis(500));
                }
            },
        }
    }
}

/// Resource the host App carries when broadcasting. `last_send` is the
/// rate-limit gate read by `broadcast_lan_advert_system`.
#[derive(Resource)]
pub struct LanAdvertiser {
    socket: UdpSocket,
    game_name: String,
    host_name: String,
    game_port: u16,
    world_seed: u64,
    max_players: u8,
    last_send: Instant,
}

impl LanAdvertiser {
    /// Build a fresh advertiser socket. Returns `None` on bind/SO_BROADCAST
    /// failure — caller logs + skips broadcasting (the host is still
    /// reachable via the manual-address path).
    pub fn new(
        game_name: String,
        host_name: String,
        game_port: u16,
        world_seed: u64,
        max_players: u8,
    ) -> Option<Self> {
        // Bind to ephemeral port — we only send, never receive on this
        // socket. Sending into UDP broadcast needs `SO_BROADCAST`.
        let socket =
            match UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)) {
                Ok(s) => s,
                Err(err) => {
                    warn!("LAN advertiser bind failed ({err}); host won't be discoverable");
                    return None;
                }
            };
        if let Err(err) = socket.set_broadcast(true) {
            warn!("LAN advertiser SO_BROADCAST failed ({err}); host won't be discoverable");
            return None;
        }
        Some(Self {
            socket,
            game_name,
            host_name,
            game_port,
            world_seed,
            max_players,
            last_send: Instant::now() - BROADCAST_INTERVAL,
        })
    }

    pub fn maybe_broadcast(&mut self, players: u8, phase: AdvertPhase) {
        let now = Instant::now();
        if now.duration_since(self.last_send) < BROADCAST_INTERVAL {
            return;
        }
        let advert = LanAdvert {
            protocol_version: crate::net::protocol::PROTOCOL_VERSION,
            game_name: self.game_name.clone(),
            host_name: self.host_name.clone(),
            game_port: self.game_port,
            players,
            max_players: self.max_players,
            phase,
            world_seed: self.world_seed,
        };
        let bytes = match bincode::serialize(&advert) {
            Ok(b) => b,
            Err(err) => {
                warn!("LAN advert serialize failed: {err}");
                return;
            }
        };
        let dest = SocketAddrV4::new(Ipv4Addr::BROADCAST, LAN_PORT);
        if let Err(err) = self.socket.send_to(&bytes, dest) {
            warn!("LAN advert send failed: {err}");
        }
        self.last_send = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lan_advert_round_trip() {
        let advert = LanAdvert {
            protocol_version: 6,
            game_name: "Test".into(),
            host_name: "host".into(),
            game_port: 5000,
            players: 1,
            max_players: 4,
            phase: AdvertPhase::Lobby,
            world_seed: 42,
        };
        let bytes = bincode::serialize(&advert).unwrap();
        assert!(bytes.len() < 512, "advert must fit a UDP packet without fragmentation");
        let back: LanAdvert = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back.protocol_version, 6);
        assert_eq!(back.game_name, "Test");
        assert_eq!(back.game_port, 5000);
        assert_eq!(back.players, 1);
        assert_eq!(back.phase, AdvertPhase::Lobby);
    }

    #[test]
    fn browser_evicts_stale_entries() {
        let browser = LanBrowser::default();
        let stale_addr = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 10), 5000);
        let stale_advert = LanAdvert {
            protocol_version: 6,
            game_name: "Old".into(),
            host_name: "host".into(),
            game_port: 5000,
            players: 0,
            max_players: 4,
            phase: AdvertPhase::Lobby,
            world_seed: 0,
        };
        {
            let mut g = browser.inner.lock().unwrap();
            g.hosts.insert(
                stale_addr,
                DiscoveredHost {
                    advert: stale_advert,
                    host_addr: stale_addr,
                    // Simulate an advert older than the TTL.
                    last_seen: Instant::now() - LISTEN_TTL - Duration::from_secs(1),
                },
            );
        }
        let live = browser.fresh();
        assert!(live.is_empty(), "stale entry must be evicted by fresh()");
    }
}
