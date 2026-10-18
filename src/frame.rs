//! MAVLink frame boundaries and header fields. Payloads are never decoded here.

pub const STX_V1: u8 = 0xFE;
pub const STX_V2: u8 = 0xFD;
const V2_SIGNED: u8 = 0x01;
const SIGNATURE_LEN: usize = 13;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub seq: u8,
    pub sysid: u8,
    pub compid: u8,
    pub msgid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Crc {
    Valid,
    Invalid,
    /// Message id not in our dialect; can't be checked.
    Unknown,
}

#[derive(Debug)]
pub struct Frame<'a> {
    pub bytes: &'a [u8],
    pub header: FrameHeader,
    pub payload: &'a [u8],
    pub crc: Crc,
    pub v2: bool,
}

/// Splits a datagram into frames. Bytes before a start marker are skipped; a truncated tail is ignored.
pub fn frames(datagram: &[u8]) -> Vec<Frame<'_>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < datagram.len() {
        if datagram[i] != STX_V1 && datagram[i] != STX_V2 {
            i += 1;
            continue;
        }
        match parse_one(&datagram[i..]) {
            Some(f) => {
                i += f.bytes.len();
                out.push(f);
            }
            None => break,
        }
    }
    out
}

/// Parses the frame starting at `buf[0]`, or `None` if it isn't one or is cut short.
pub fn parse_one(buf: &[u8]) -> Option<Frame<'_>> {
    let (v2, hdr_len) = match *buf.first()? {
        STX_V1 => (false, 6),
        STX_V2 => (true, 10),
        _ => return None,
    };
    if buf.len() < hdr_len {
        return None;
    }
    let len = buf[1] as usize;
    let (header, sig) = if v2 {
        let sig = if buf[2] & V2_SIGNED != 0 {
            SIGNATURE_LEN
        } else {
            0
        };
        let msgid = u32::from_le_bytes([buf[7], buf[8], buf[9], 0]);
        (
            FrameHeader {
                seq: buf[4],
                sysid: buf[5],
                compid: buf[6],
                msgid,
            },
            sig,
        )
    } else {
        (
            FrameHeader {
                seq: buf[2],
                sysid: buf[3],
                compid: buf[4],
                msgid: buf[5] as u32,
            },
            0,
        )
    };
    let bytes = buf.get(..hdr_len + len + 2 + sig)?;
    let crc_at = hdr_len + len;
    let got = u16::from_le_bytes([bytes[crc_at], bytes[crc_at + 1]]);
    let crc = match crc_extra(header.msgid) {
        Some(extra) if x25(&bytes[1..crc_at], extra) == got => Crc::Valid,
        Some(_) => Crc::Invalid,
        None => Crc::Unknown,
    };
    Some(Frame {
        bytes,
        header,
        payload: &bytes[hdr_len..crc_at],
        crc,
        v2,
    })
}

/// CRC-16/MCRF4XX over `data`, then the message's CRC_EXTRA byte.
pub fn x25(data: &[u8], extra: u8) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data.iter().chain(std::iter::once(&extra)) {
        let mut t = b ^ (crc as u8);
        t ^= t << 4;
        crc = (crc >> 8) ^ ((t as u16) << 8) ^ ((t as u16) << 3) ^ ((t as u16) >> 4);
    }
    crc
}

/// CRC_EXTRA for `msgid`, or `None` when the ardupilotmega dialect doesn't know it.
pub fn crc_extra(msgid: u32) -> Option<u8> {
    use mavlink::{Message, dialects::ardupilotmega::MavMessage};
    MavMessage::default_message_from_id(msgid).map(|_| MavMessage::extra_crc(msgid))
}

/// Frame builders for tests in this crate and in `telemetry`.
#[doc(hidden)]
pub mod testutil {
    use super::*;

    pub fn v2(seq: u8, sysid: u8, compid: u8, msgid: u32, payload: &[u8]) -> Vec<u8> {
        let id = msgid.to_le_bytes();
        let mut f = vec![
            STX_V2,
            payload.len() as u8,
            0,
            0,
            seq,
            sysid,
            compid,
            id[0],
            id[1],
            id[2],
        ];
        f.extend_from_slice(payload);
        let crc = x25(&f[1..], crc_extra(msgid).unwrap_or(0));
        f.extend_from_slice(&crc.to_le_bytes());
        f
    }

    /// HEARTBEAT payload: custom_mode u32, type, autopilot, base_mode, system_status, mavlink_version.
    pub fn heartbeat(seq: u8, sysid: u8, compid: u8, mav_type: u8, autopilot: u8) -> Vec<u8> {
        v2(
            seq,
            sysid,
            compid,
            0,
            &[0, 0, 0, 0, mav_type, autopilot, 0, 0, 3],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{testutil::*, *};

    #[test]
    fn x25_matches_check_value() {
        // CRC-16/MCRF4XX check value; MAVLink appends CRC_EXTRA as the last byte.
        assert_eq!(x25(b"12345678", b'9'), 0x6F91);
    }

    #[test]
    fn crc_extra_known_and_unknown() {
        assert_eq!(crc_extra(0), Some(50)); // HEARTBEAT
        assert_eq!(crc_extra(241), Some(90)); // VIBRATION
        assert_eq!(crc_extra(193), Some(71)); // EKF_STATUS_REPORT
        assert_eq!(crc_extra(60_000), None);
    }

    #[test]
    fn parses_v2_heartbeat() {
        let hb = heartbeat(7, 1, 1, 2, 3);
        let fs = frames(&hb);
        assert_eq!(fs.len(), 1);
        assert_eq!(
            fs[0].header,
            FrameHeader {
                seq: 7,
                sysid: 1,
                compid: 1,
                msgid: 0
            }
        );
        assert_eq!(fs[0].crc, Crc::Valid);
        assert!(fs[0].v2);
        assert_eq!(fs[0].bytes, &hb[..]);
    }

    #[test]
    fn parses_v1_frame() {
        let payload = [0, 0, 0, 0, 2, 3, 0, 0, 3];
        let mut f = vec![STX_V1, payload.len() as u8, 9, 1, 1, 0];
        f.extend_from_slice(&payload);
        let crc = x25(&f[1..], 50);
        f.extend_from_slice(&crc.to_le_bytes());
        let fs = frames(&f);
        assert_eq!(fs.len(), 1);
        assert_eq!(fs[0].header.seq, 9);
        assert_eq!(fs[0].crc, Crc::Valid);
        assert!(!fs[0].v2);
    }

    #[test]
    fn splits_two_frames_in_one_datagram() {
        let mut d = heartbeat(1, 1, 1, 2, 3);
        d.extend(heartbeat(2, 1, 1, 2, 3));
        let seqs: Vec<u8> = frames(&d).iter().map(|f| f.header.seq).collect();
        assert_eq!(seqs, [1, 2]);
    }

    #[test]
    fn skips_signature_on_signed_frame() {
        let mut d = vec![STX_V2, 9, 0x01, 0, 7, 1, 1, 0, 0, 0];
        d.extend([0, 0, 0, 0, 2, 3, 0, 0, 3]);
        let crc = x25(&d[1..], 50);
        d.extend(crc.to_le_bytes());
        d.extend([0xAA; 13]);
        d.extend(heartbeat(8, 1, 1, 2, 3));
        let fs = frames(&d);
        assert_eq!(fs.len(), 2);
        assert_eq!(fs[0].crc, Crc::Valid);
        assert_eq!(fs[0].bytes.len(), 10 + 9 + 2 + 13);
        assert_eq!(fs[1].header.seq, 8);
    }

    #[test]
    fn flags_bad_crc_and_unknown_ids() {
        let mut bad = heartbeat(1, 1, 1, 2, 3);
        bad[12] ^= 0xFF;
        assert_eq!(frames(&bad)[0].crc, Crc::Invalid);
        assert_eq!(
            frames(&v2(1, 1, 1, 60_000, &[1, 2, 3]))[0].crc,
            Crc::Unknown
        );
    }

    #[test]
    fn skips_leading_garbage_and_truncated_tail() {
        let mut d = vec![0x00, 0x11];
        d.extend(heartbeat(3, 1, 1, 2, 3));
        d.extend(&heartbeat(4, 1, 1, 2, 3)[..6]);
        let fs = frames(&d);
        assert_eq!(fs.len(), 1);
        assert_eq!(fs[0].header.seq, 3);
    }
}
