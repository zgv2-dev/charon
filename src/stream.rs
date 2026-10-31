//! Cuts MAVLink frames out of a byte stream (TCP, serial), where reads split and join frames anywhere.

use crate::frame::{Crc, STX_V1, STX_V2, parse_one};

/// A frame is at most 280 bytes (v2, 255 payload, signed); more than this buffered without one is noise.
const MAX_PENDING: usize = 4096;
/// Frames read together go on as datagrams up to this size, safely under an Ethernet MTU.
pub const DATAGRAM_MAX: usize = 1200;

#[derive(Default)]
pub struct Deframer {
    pending: Vec<u8>,
    /// Bytes dropped that weren't part of a frame, and frames dropped for a bad CRC.
    pub skipped: u64,
}

impl Deframer {
    /// Adds what was read and returns the frames now complete, packed into datagrams of whole frames.
    pub fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.pending.extend_from_slice(data);
        let mut out: Vec<Vec<u8>> = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            let b = self.pending[i];
            if b != STX_V1 && b != STX_V2 {
                i += 1;
                self.skipped += 1;
                continue;
            }
            match parse_one(&self.pending[i..]) {
                // cut short: wait for the rest
                None => break,
                // a start byte inside noise: resync on the next one
                Some(f) if f.crc == Crc::Invalid => {
                    i += 1;
                    self.skipped += 1;
                }
                Some(f) => {
                    let n = f.bytes.len();
                    match out.last_mut() {
                        Some(d) if d.len() + n <= DATAGRAM_MAX => d.extend_from_slice(f.bytes),
                        _ => out.push(f.bytes.to_vec()),
                    }
                    i += n;
                }
            }
        }
        self.pending.drain(..i);
        if self.pending.len() > MAX_PENDING {
            self.skipped += self.pending.len() as u64;
            self.pending.clear();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::testutil::{heartbeat, v2};

    #[test]
    fn joins_frames_split_across_reads() {
        let hb = heartbeat(1, 1, 1, 2, 3);
        let mut d = Deframer::default();
        assert!(d.push(&hb[..5]).is_empty());
        assert!(d.push(&hb[5..12]).is_empty());
        assert_eq!(d.push(&hb[12..]), [hb]);
    }

    #[test]
    fn packs_frames_read_together_into_one_datagram() {
        let (a, b) = (heartbeat(1, 1, 1, 2, 3), heartbeat(2, 1, 1, 2, 3));
        let mut d = Deframer::default();
        assert_eq!(d.push(&[a.clone(), b.clone()].concat()), [[a, b].concat()]);
    }

    #[test]
    fn keeps_datagrams_under_the_mtu() {
        let big = v2(0, 1, 1, 60_000, &[7; 250]);
        let mut d = Deframer::default();
        let out = d.push(&big.repeat(10));
        assert!(out.len() > 1);
        assert!(out.iter().all(|g| g.len() <= DATAGRAM_MAX));
        assert_eq!(out.concat(), big.repeat(10));
    }

    #[test]
    fn skips_noise_and_bad_frames_and_resyncs() {
        let hb = heartbeat(4, 1, 1, 2, 3);
        let mut bad = heartbeat(3, 1, 1, 2, 3);
        bad[12] ^= 0xFF;
        let mut d = Deframer::default();
        let out = d.push(&[&[0x00, 0x42][..], &bad, &hb].concat());
        assert_eq!(out, [hb]);
        assert!(d.skipped > 2);
    }

    #[test]
    fn unknown_messages_pass() {
        let f = v2(0, 1, 1, 60_000, &[1, 2, 3]);
        assert_eq!(Deframer::default().push(&f), [f]);
    }

    #[test]
    fn gives_up_on_endless_noise() {
        let mut d = Deframer::default();
        // a start byte with a length that never arrives, followed by a lot of nothing
        d.push(&[STX_V2, 255]);
        d.push(&[0u8; MAX_PENDING]);
        assert!(d.pending.is_empty());
        assert_eq!(d.push(&heartbeat(1, 1, 1, 2, 3)).len(), 1);
    }
}
