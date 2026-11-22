//! RTP H.264 -> Annex-B access unit reassembly (receiver side).
//!
//! Phase 0 uses the RTP marker bit as the frame boundary and drops any frame
//! with a sequence gap, asking the sender for a keyframe instead of
//! concealing errors. A proper jitter buffer (reordering, NACK-aware waiting)
//! is a Phase 1 item.

use bytes::{Bytes, BytesMut};
use rtc::rtp::Packet;
use rtc::rtp::codec::h264::H264Packet;
use rtc::rtp::packetizer::Depacketizer;

pub(crate) enum Assembled {
    Frame(Bytes),
    /// Packets were lost; the caller should request a keyframe.
    Loss,
}

pub(crate) struct FrameAssembler {
    depacketizer: H264Packet,
    buffer: BytesMut,
    expected_seq: Option<u16>,
    timestamp: Option<u32>,
    corrupt: bool,
    /// After a loss (and at startup) frames are dropped until an IDR arrives,
    /// since P-frames would reference pictures the decoder never got.
    waiting_for_keyframe: bool,
}

/// True if the Annex-B access unit contains an IDR slice (NAL type 5).
pub(crate) fn contains_idr(annexb: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < annexb.len() {
        if annexb[i] == 0 && annexb[i + 1] == 0 && annexb[i + 2] == 1 {
            if annexb[i + 3] & 0x1F == 5 {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

impl FrameAssembler {
    pub fn new() -> Self {
        Self {
            depacketizer: H264Packet::default(),
            buffer: BytesMut::new(),
            expected_seq: None,
            timestamp: None,
            corrupt: false,
            waiting_for_keyframe: true,
        }
    }

    fn reset_frame(&mut self) {
        self.buffer.clear();
        self.depacketizer = H264Packet::default();
        self.timestamp = None;
    }

    pub fn push(&mut self, packet: &Packet) -> Option<Assembled> {
        let seq = packet.header.sequence_number;
        let mut lost = false;
        if let Some(expected) = self.expected_seq {
            let delta = seq.wrapping_sub(expected);
            if delta >= 0x8000 {
                // Late duplicate / retransmission of an already-handled packet.
                return None;
            }
            if delta != 0 {
                lost = true;
            }
        }
        self.expected_seq = Some(seq.wrapping_add(1));

        if lost {
            self.reset_frame();
            self.corrupt = true;
        }
        if self.timestamp.is_some_and(|ts| ts != packet.header.timestamp) {
            // New frame started without the previous marker: previous one is incomplete.
            self.reset_frame();
            self.corrupt = true;
        }
        self.timestamp = Some(packet.header.timestamp);

        match self.depacketizer.depacketize(&packet.payload) {
            Ok(nal) => self.buffer.extend_from_slice(&nal),
            Err(e) => {
                tracing::debug!("depacketize error: {e}");
                self.corrupt = true;
            }
        }

        if !packet.header.marker {
            return lost.then_some(Assembled::Loss);
        }

        let frame = self.buffer.split().freeze();
        let was_corrupt = std::mem::take(&mut self.corrupt);
        self.reset_frame();
        if was_corrupt || lost {
            self.waiting_for_keyframe = true;
            return Some(Assembled::Loss);
        }
        if self.waiting_for_keyframe {
            if !contains_idr(&frame) {
                return None;
            }
            self.waiting_for_keyframe = false;
        }
        Some(Assembled::Frame(frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtc::rtp::Header;

    fn pkt(seq: u16, ts: u32, marker: bool, nal_type: u8) -> Packet {
        Packet {
            header: Header { sequence_number: seq, timestamp: ts, marker, ..Default::default() },
            payload: Bytes::from(vec![nal_type, 0xAA, 0xBB]),
        }
    }

    #[test]
    fn waits_for_idr_then_passes_frames() {
        let mut a = FrameAssembler::new();
        assert!(a.push(&pkt(1, 100, true, 1)).is_none()); // P-frame before IDR: dropped
        assert!(matches!(a.push(&pkt(2, 200, true, 5)), Some(Assembled::Frame(_))));
        assert!(matches!(a.push(&pkt(3, 300, true, 1)), Some(Assembled::Frame(_))));
    }

    #[test]
    fn gap_reports_loss_and_resyncs_on_idr() {
        let mut a = FrameAssembler::new();
        assert!(matches!(a.push(&pkt(1, 100, true, 5)), Some(Assembled::Frame(_))));
        assert!(matches!(a.push(&pkt(3, 200, true, 1)), Some(Assembled::Loss))); // seq 2 lost
        assert!(a.push(&pkt(4, 300, true, 1)).is_none()); // still waiting for IDR
        assert!(matches!(a.push(&pkt(5, 400, true, 5)), Some(Assembled::Frame(_))));
    }
}
