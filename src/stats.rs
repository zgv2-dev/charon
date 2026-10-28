//! Heartbeat and loss tracking for every vehicle heard, and the health of the one followed. Fed every datagram the
//! router sees.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use crate::frame::{Crc, frames};

const MAV_TYPE_GCS: u8 = 6;
const MAV_AUTOPILOT_INVALID: u8 = 8;
const HEARTBEAT_LATE: Duration = Duration::from_millis(1500);
pub const HEARTBEAT_LOST: Duration = Duration::from_secs(3);
const LOSS_RECENT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Health {
    #[default]
    NoSource,
    Ok,
    Degraded,
    Lost,
}

#[derive(Debug, Clone)]
pub struct VehicleStatus {
    pub sysid: u8,
    pub mav_type: u8,
    pub heartbeat_age: Duration,
    pub received: u64,
    pub lost: u64,
    /// Every component heard on this sysid: the autopilot, a camera, a companion computer.
    pub components: Vec<u8>,
}

/// One (sysid, compid) pair: MAVLink sequence numbers count per component.
#[derive(Default)]
struct Link {
    last_seq: Option<u8>,
    received: u64,
    lost: u64,
    last_loss: Option<Instant>,
}

/// A vehicle's autopilot, from its heartbeats.
#[derive(Clone, Copy)]
struct Heard {
    compid: u8,
    mav_type: u8,
    last: Instant,
}

#[derive(Default)]
pub struct Stats {
    links: HashMap<(u8, u8), Link>,
    vehicles: HashMap<u8, Heard>,
    /// The sysid of the latest vehicle heartbeat.
    latest: Option<u8>,
    /// Set by the app; None follows `latest`.
    follow: Option<u8>,
    pub crc_errors: u64,
}

impl Stats {
    pub fn ingest(&mut self, datagram: &[u8], now: Instant) {
        for f in frames(datagram) {
            if f.crc == Crc::Invalid {
                self.crc_errors += 1;
                continue;
            }
            let h = f.header;
            let link = self.links.entry((h.sysid, h.compid)).or_default();
            link.received += 1;
            if let Some(prev) = link.last_seq {
                let gap = h.seq.wrapping_sub(prev);
                // gap 0 is a duplicate; gaps over 128 look like a reboot or reorder, not loss
                if (2..=128).contains(&gap) {
                    link.lost += (gap - 1) as u64;
                    link.last_loss = Some(now);
                }
            }
            link.last_seq = Some(h.seq);

            if h.msgid == 0 {
                let at = |i: usize| f.payload.get(i).copied().unwrap_or(0); // v2 trims trailing zeros
                let (mav_type, autopilot) = (at(4), at(5));
                if mav_type != MAV_TYPE_GCS && autopilot != MAV_AUTOPILOT_INVALID {
                    if !self.vehicles.contains_key(&h.sysid) {
                        tracing::info!(sysid = h.sysid, mav_type, "vehicle seen");
                    }
                    let heard = Heard {
                        compid: h.compid,
                        mav_type,
                        last: now,
                    };
                    self.vehicles.insert(h.sysid, heard);
                    self.latest = Some(h.sysid);
                }
            }
        }
    }

    /// Whose health counts: `Some(sysid)` once it is heard, None for whichever vehicle was heard last.
    pub fn set_follow(&mut self, sysid: Option<u8>) {
        self.follow = sysid;
    }

    /// The followed vehicle, or the latest heard while the followed one hasn't been.
    fn followed(&self) -> Option<(u8, Heard)> {
        let sysid = self
            .follow
            .filter(|s| self.vehicles.contains_key(s))
            .or(self.latest)?;
        Some((sysid, self.vehicles[&sysid]))
    }

    fn status(&self, sysid: u8, heard: Heard, now: Instant) -> VehicleStatus {
        let (received, lost) = self
            .links
            .get(&(sysid, heard.compid))
            .map_or((0, 0), |k| (k.received, k.lost));
        let mut components: Vec<u8> = self
            .links
            .keys()
            .filter(|(s, _)| *s == sysid)
            .map(|(_, c)| *c)
            .collect();
        components.sort_unstable();
        VehicleStatus {
            sysid,
            mav_type: heard.mav_type,
            heartbeat_age: now.saturating_duration_since(heard.last),
            received,
            lost,
            components,
        }
    }

    pub fn vehicle(&self, now: Instant) -> Option<VehicleStatus> {
        let (sysid, heard) = self.followed()?;
        Some(self.status(sysid, heard, now))
    }

    /// Every vehicle heard, by sysid.
    pub fn vehicles(&self, now: Instant) -> Vec<VehicleStatus> {
        let mut all: Vec<_> = self
            .vehicles
            .iter()
            .map(|(&sysid, &heard)| self.status(sysid, heard, now))
            .collect();
        all.sort_by_key(|v| v.sysid);
        all
    }

    pub fn health(&self, now: Instant) -> Health {
        let Some((sysid, Heard { compid, last, .. })) = self.followed() else {
            return Health::NoSource;
        };
        let age = now.saturating_duration_since(last);
        // Only the autopilot's own sequence: a companion on the same sysid (seen: compid 191)
        // can interleave several sequence counters, which reads as constant loss.
        let recent_loss = self.links.get(&(sysid, compid)).is_some_and(|l| {
            l.last_loss
                .is_some_and(|t| now.saturating_duration_since(t) < LOSS_RECENT)
        });
        if age >= HEARTBEAT_LOST {
            Health::Lost
        } else if age > HEARTBEAT_LATE || recent_loss {
            Health::Degraded
        } else {
            Health::Ok
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::testutil::{heartbeat, v2};

    fn feed(stats: &mut Stats, seqs: &[u8], now: Instant) {
        for &s in seqs {
            stats.ingest(&heartbeat(s, 1, 1, 2, 3), now);
        }
    }

    #[test]
    fn autopilot_heartbeat_creates_vehicle() {
        let (mut s, now) = (Stats::default(), Instant::now());
        s.ingest(&heartbeat(0, 1, 1, 2, 3), now);
        let v = s.vehicle(now).unwrap();
        assert_eq!((v.sysid, v.mav_type), (1, 2));
    }

    #[test]
    fn gcs_and_companion_heartbeats_ignored() {
        let (mut s, now) = (Stats::default(), Instant::now());
        s.ingest(&heartbeat(0, 255, 190, 6, 8), now);
        s.ingest(&heartbeat(0, 1, 191, 18, 8), now);
        assert!(s.vehicle(now).is_none());
        assert_eq!(s.health(now), Health::NoSource);
    }

    #[test]
    fn truncated_heartbeat_still_counts() {
        // MAVLink 2 senders drop trailing zero bytes; here only custom_mode, type and autopilot arrive.
        let (mut s, now) = (Stats::default(), Instant::now());
        s.ingest(&v2(0, 1, 1, 0, &[0, 0, 0, 0, 2, 3]), now);
        assert!(s.vehicle(now).is_some());
    }

    #[test]
    fn sequence_gap_counts_loss() {
        let (mut s, now) = (Stats::default(), Instant::now());
        feed(&mut s, &[0, 1, 4], now);
        let v = s.vehicle(now).unwrap();
        assert_eq!((v.received, v.lost), (3, 2));
        assert_eq!(s.health(now), Health::Degraded);
    }

    #[test]
    fn sequence_wrap_is_not_loss() {
        let (mut s, now) = (Stats::default(), Instant::now());
        feed(&mut s, &[254, 255, 0, 1], now);
        assert_eq!(s.vehicle(now).unwrap().lost, 0);
        assert_eq!(s.health(now), Health::Ok);
    }

    #[test]
    fn reboot_jump_is_not_loss() {
        let (mut s, now) = (Stats::default(), Instant::now());
        feed(&mut s, &[100, 101, 5], now);
        assert_eq!(s.vehicle(now).unwrap().lost, 0);
    }

    #[test]
    fn duplicate_is_not_loss() {
        let (mut s, now) = (Stats::default(), Instant::now());
        feed(&mut s, &[3, 3, 4], now);
        assert_eq!(s.vehicle(now).unwrap().lost, 0);
    }

    #[test]
    fn health_follows_heartbeat_age() {
        let (mut s, t0) = (Stats::default(), Instant::now());
        s.ingest(&heartbeat(0, 1, 1, 2, 3), t0);
        assert_eq!(s.health(t0 + Duration::from_millis(1000)), Health::Ok);
        assert_eq!(s.health(t0 + Duration::from_millis(2000)), Health::Degraded);
        assert_eq!(s.health(t0 + Duration::from_secs(3)), Health::Lost);
    }

    #[test]
    fn loss_stops_being_recent() {
        let (mut s, t0) = (Stats::default(), Instant::now());
        feed(&mut s, &[0, 2], t0);
        let later = t0 + Duration::from_secs(6);
        feed(&mut s, &[3], later);
        assert_eq!(s.health(later), Health::Ok);
    }

    #[test]
    fn companion_sequence_is_not_vehicle_loss() {
        let (mut s, now) = (Stats::default(), Instant::now());
        feed(&mut s, &[0, 1, 2], now);
        for seq in [21, 104, 105, 22, 106] {
            s.ingest(&heartbeat(seq, 1, 191, 18, 8), now);
        }
        assert_eq!(s.vehicle(now).unwrap().lost, 0);
        assert_eq!(s.health(now), Health::Ok);
    }

    #[test]
    fn tracks_every_vehicle_and_follows_the_latest() {
        let (mut s, now) = (Stats::default(), Instant::now());
        s.ingest(&heartbeat(0, 2, 1, 13, 3), now);
        s.ingest(&heartbeat(0, 1, 1, 2, 3), now);
        s.ingest(&heartbeat(0, 1, 100, 30, 8), now); // its camera
        let all = s.vehicles(now);
        assert_eq!(all.iter().map(|v| v.sysid).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(all[0].components, [1, 100]);
        assert_eq!(s.vehicle(now).unwrap().sysid, 1, "heard last");
    }

    #[test]
    fn health_is_the_followed_vehicles() {
        let (mut s, t0) = (Stats::default(), Instant::now());
        s.set_follow(Some(2));
        s.ingest(&heartbeat(0, 1, 1, 2, 3), t0);
        assert_eq!(s.vehicle(t0).unwrap().sysid, 1, "2 not heard yet");
        s.ingest(&heartbeat(0, 2, 1, 2, 3), t0);
        let later = t0 + Duration::from_secs(4);
        s.ingest(&heartbeat(1, 1, 1, 2, 3), later);
        assert_eq!(s.vehicle(later).unwrap().sysid, 2);
        assert_eq!(s.health(later), Health::Lost, "1 is fine, 2 went quiet");
        s.set_follow(None);
        assert_eq!(s.health(later), Health::Ok);
    }

    #[test]
    fn bad_crc_counted_not_ingested() {
        let (mut s, now) = (Stats::default(), Instant::now());
        let mut bad = heartbeat(0, 1, 1, 2, 3);
        bad[12] ^= 0xFF;
        s.ingest(&bad, now);
        assert_eq!(s.crc_errors, 1);
        assert!(s.vehicle(now).is_none());
    }
}
