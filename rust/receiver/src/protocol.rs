use anyhow::{bail, ensure, Context, Result};
use des::{
    cipher::{generic_array::GenericArray, BlockDecrypt, BlockEncrypt, KeyInit},
    TdesEde3,
};
use serde_json::{json, Value};
use std::collections::VecDeque;

const WIRE_KEY: [u8; 24] = [
    0x41, 0x07, 0xdf, 0x53, 0x13, 0x2d, 0x81, 0x05, 0x17, 0xb8, 0x5f, 0xa0, 0xd8, 0x44, 0x44, 0x93,
    0x33, 0x98, 0x63, 0x44, 0xd6, 0x19, 0xbf, 0xc3,
];
pub const AUDIO_HEADER_SIZE: usize = 56;

#[derive(Debug)]
pub struct Control {
    pub name: String,
    pub value: Value,
}

pub fn encode(value: Value, name: &str, category: u32) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(
        &json!({"name":name,"category":category,"priprity":1,"seq":0,"timestamp":0,"value":value.to_string()}),
    )?;
    bytes.resize(bytes.len() + 8 - bytes.len() % 8, 0);
    let cipher = TdesEde3::new_from_slice(&WIRE_KEY).unwrap();
    for block in bytes.chunks_exact_mut(8) {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
    Ok(bytes)
}

#[derive(Default)]
pub struct ControlDecoder {
    encrypted: Vec<u8>,
    plain: Vec<u8>,
}
impl ControlDecoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Control>> {
        ensure!(
            self.encrypted.len() + bytes.len() <= 1_048_576,
            "control frame exceeds 1 MiB"
        );
        self.encrypted.extend_from_slice(bytes);
        let complete = self.encrypted.len() / 8 * 8;
        let cipher = TdesEde3::new_from_slice(&WIRE_KEY).unwrap();
        for block in self.encrypted[..complete].chunks_exact(8) {
            let mut block = GenericArray::clone_from_slice(block);
            cipher.decrypt_block(&mut block);
            self.plain.extend_from_slice(&block);
        }
        self.encrypted.drain(..complete);
        ensure!(
            self.plain.len() <= 1_048_576,
            "control message exceeds 1 MiB"
        );
        let mut messages = Vec::new();
        while let Some(end) = self.plain.iter().position(|b| *b == 0) {
            if end != 0 {
                let outer: Value = serde_json::from_slice(&self.plain[..end])
                    .context("malformed control envelope")?;
                let value: Value = serde_json::from_str(
                    outer["value"].as_str().context("missing control value")?,
                )?;
                ensure!(value.is_object(), "control value is not an object");
                messages.push(Control {
                    name: outer["name"]
                        .as_str()
                        .context("missing control name")?
                        .into(),
                    value,
                });
            }
            self.plain.drain(..end + 1);
        }
        ensure!(
            self.plain.is_empty() || self.plain[0] == b'{',
            "malformed control envelope prefix"
        );
        Ok(messages)
    }
}

#[derive(Debug, Clone)]
pub struct AudioPacket {
    pub rate: u32,
    pub position: u64,
    pub reference_position: u64,
    pub reference_ns: u64,
    pub payload: Vec<u8>,
}
impl AudioPacket {
    pub fn presentation_ns(&self, leadtime: u32) -> Result<i64> {
        ensure!(self.rate != 0, "zero audio sample rate");
        let delta = (i128::from(self.position) - i128::from(self.reference_position))
            * 1_000_000_000
            / i128::from(self.rate);
        (i128::from(self.reference_ns) + delta + i128::from(leadtime) * 1_000_000)
            .try_into()
            .context("presentation time overflow")
    }
}

#[derive(Default)]
pub struct AudioDecoder {
    buffer: Vec<u8>,
}
impl AudioDecoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<AudioPacket>> {
        self.buffer.extend_from_slice(bytes);
        ensure!(
            self.buffer.len() < 1_048_576,
            "audio framing buffer exceeds 1 MiB"
        );
        let mut packets = Vec::new();
        let mut offset = 0;
        while self.buffer.len() - offset >= AUDIO_HEADER_SIZE {
            let b = &self.buffer[offset..];
            ensure!(
                u32::from_le_bytes(b[..4].try_into().unwrap()) == 0xfedcba98,
                "invalid WMRM audio magic"
            );
            let size = u16::from_le_bytes(b[4..6].try_into().unwrap()) as usize;
            ensure!(size >= AUDIO_HEADER_SIZE, "invalid WMRM audio frame size");
            if b.len() < size {
                break;
            }
            let kind = u16::from_le_bytes(b[6..8].try_into().unwrap());
            let format = u32::from_le_bytes(b[8..12].try_into().unwrap());
            let rate = (format >> 8) & 0xffff;
            ensure!(
                kind == 10 && format & 15 == 2 && [32000, 44100, 48000].contains(&rate),
                "unsupported WMRM codec/format: codec={kind}, format={format:x}"
            );
            packets.push(AudioPacket {
                rate,
                position: u64::from_le_bytes(b[16..24].try_into().unwrap()),
                reference_position: u64::from_le_bytes(b[24..32].try_into().unwrap()),
                reference_ns: u64::from_le_bytes(b[32..40].try_into().unwrap()),
                payload: b[AUDIO_HEADER_SIZE..size].to_vec(),
            });
            offset += size;
        }
        self.buffer.drain(..offset);
        Ok(packets)
    }
}

pub fn clock_request(sequence: u32, sent: u64) -> [u8; 32] {
    let mut b = [0; 32];
    b[..4].copy_from_slice(&sequence.to_le_bytes());
    b[8..16].copy_from_slice(&sent.to_le_bytes());
    b
}

#[derive(Default)]
pub struct LeaderClock {
    samples: VecDeque<(u64, i64)>,
}
impl LeaderClock {
    pub fn observe(
        &mut self,
        b: &[u8; 32],
        sequence: u32,
        sent: u64,
        received: u64,
    ) -> Result<(i64, u64)> {
        ensure!(
            u32::from_le_bytes(b[..4].try_into().unwrap()) == sequence
                && u32::from_le_bytes(b[4..8].try_into().unwrap()) == 1
                && u64::from_le_bytes(b[8..16].try_into().unwrap()) == sent
                && received >= sent,
            "unmatched group clock reply"
        );
        let leader = u64::from_le_bytes(b[16..24].try_into().unwrap());
        let offset = (i128::from(leader) - (i128::from(sent) + i128::from(received)) / 2)
            .try_into()
            .context("clock offset overflow")?;
        if self.samples.len() == 30 {
            self.samples.pop_front();
        }
        self.samples.push_back((received - sent, offset));
        let &(rtt, offset) = self.samples.iter().min_by_key(|(rtt, _)| rtt).unwrap();
        Ok((offset, rtt))
    }
}

/// Split ADTS access units, returning raw AAC for Symphonia and ASC codec config.
pub fn adts_frames(payload: &[u8]) -> Result<Vec<(&[u8], [u8; 2])>> {
    let mut out = Vec::new();
    let mut offset = 0;
    while offset < payload.len() {
        let b = &payload[offset..];
        ensure!(
            b.len() >= 7 && b[0] == 0xff && b[1] & 0xf6 == 0xf0,
            "invalid ADTS header"
        );
        let header = if b[1] & 1 == 1 { 7 } else { 9 };
        let length = ((b[3] as usize & 3) << 11) | ((b[4] as usize) << 3) | (b[5] as usize >> 5);
        ensure!(
            length >= header && length <= b.len(),
            "truncated ADTS frame"
        );
        ensure!(
            b[6] & 3 == 0,
            "multiple raw AAC blocks per ADTS frame are unsupported"
        );
        let object = (b[2] >> 6) + 1;
        let rate = (b[2] >> 2) & 15;
        let channels = ((b[2] & 1) << 2) | (b[3] >> 6);
        if channels != 2 || rate >= 13 {
            bail!("unsupported AAC channel/sample-rate configuration");
        }
        out.push((
            &b[header..length],
            [
                (object << 3) | (rate >> 1),
                ((rate & 1) << 7) | (channels << 3),
            ],
        ));
        offset += length;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn audio_fragmentation_preserves_payload_and_presentation_time() {
        let payload = b"synthetic AAC framing fixture";
        let mut wire = vec![0; AUDIO_HEADER_SIZE];
        wire[..4].copy_from_slice(&0xfedcba98u32.to_le_bytes());
        wire[4..6].copy_from_slice(&((AUDIO_HEADER_SIZE + payload.len()) as u16).to_le_bytes());
        wire[6..8].copy_from_slice(&10u16.to_le_bytes());
        wire[8..12].copy_from_slice(&0x01ac4422u32.to_le_bytes());
        wire[16..24].copy_from_slice(&1024u64.to_le_bytes());
        wire[24..32].copy_from_slice(&2048u64.to_le_bytes());
        wire[32..40].copy_from_slice(&10_000_000_000u64.to_le_bytes());
        wire.extend_from_slice(payload);
        for fragment in [1, 3, 55, 56, 57, 65536] {
            let mut decoder = AudioDecoder::default();
            let mut packets = Vec::new();
            for part in wire.chunks(fragment) {
                packets.extend(decoder.feed(part).unwrap());
            }
            assert_eq!(packets.len(), 1);
            assert_eq!(packets[0].payload, payload);
            assert_eq!(packets[0].rate, 44100);
            assert_eq!(
                packets[0].presentation_ns(800).unwrap(),
                10_800_000_000 - 1024 * 1_000_000_000 / 44100
            );
        }
        wire[0] = 0;
        assert!(AudioDecoder::default().feed(&wire).is_err());
        assert!(adts_frames(b"truncated").is_err());
    }
    #[test]
    fn captured_heartbeat_and_fragmented_unicode_roundtrip() {
        let capture = hex::decode(concat!(
            "03b4ec8c48b5ee8ccd167c993f15dcc677b0b997de411f4ca3334de728c9703ac1e2",
            "62db00d384efab99b4a4f6775c79d039b5196ee9fad447233134dc7f25ca22459d945",
            "bf22051da389fc01a04c3f8b85c0bb7c927fc26062b5d57bcaf3fc63edd8d8778038",
            "39b2be6d0c2ee69218c7cbe08cab5a23ac851600ea1d176da674a915f40a0ea65267",
            "5133c926a63bf3466b3fbe3b94078a937dca93cdc43e2837988f02f633d24d2"
        ))
        .unwrap();
        for fragment in [1, 3, 7, 8, 9, 17, 4096] {
            let mut decoder = ControlDecoder::default();
            let mut out = Vec::new();
            for part in capture.chunks(fragment) {
                out.extend(decoder.feed(part).unwrap());
            }
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].name, "_heart_tick");
            assert_eq!(out[0].value["render_pts"], 25336832);
        }
        let value = json!({"cmd":"login","name":"Mac 🎵 \"test\""});
        assert_eq!(
            ControlDecoder::default()
                .feed(&encode(value.clone(), "_linkplay_mrm", 253).unwrap())
                .unwrap()[0]
                .value,
            value
        );
    }
    #[test]
    fn validates_clock_echo_and_uses_best_rtt() {
        let mut clock = LeaderClock::default();
        let mut reply = clock_request(7, 1000);
        reply[4..8].copy_from_slice(&1u32.to_le_bytes());
        reply[16..24].copy_from_slice(&1110u64.to_le_bytes());
        assert_eq!(clock.observe(&reply, 7, 1000, 1020).unwrap(), (100, 20));
        assert!(clock.observe(&reply, 8, 1000, 1020).is_err());
        let mut slower = clock_request(8, 2000);
        slower[4..8].copy_from_slice(&1u32.to_le_bytes());
        slower[16..24].copy_from_slice(&2250u64.to_le_bytes());
        assert_eq!(clock.observe(&slower, 8, 2000, 2200).unwrap(), (100, 20));
    }
}
