//! RFC 7692 compression, context takeover, and negotiated window sizes.
use crate::{Error, Result, codec::Frame};
use bytes::{Bytes, BytesMut};
use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress};
use std::collections::BTreeMap;

/// Negotiated directional compression parameters.
#[derive(Clone, Copy, Debug)]
pub struct Parameters {
    /// Local compressor window, from 9 through 15 bits.
    pub tx_window: u8,
    /// Peer compressor window, from 9 through 15 bits.
    pub rx_window: u8,
    /// Reset the compressor between messages.
    pub tx_no_context: bool,
    /// Reset the decompressor between messages.
    pub rx_no_context: bool,
}
impl Default for Parameters {
    fn default() -> Self {
        Self {
            tx_window: 15,
            rx_window: 15,
            tx_no_context: false,
            rx_no_context: false,
        }
    }
}

fn parse(value: &str) -> Result<BTreeMap<&str, Option<&str>>> {
    let mut parts = value.split(';').map(str::trim);
    if parts.next() != Some("permessage-deflate") {
        return Err(Error::Handshake("unknown extension".into()));
    }
    let mut map = BTreeMap::new();
    for part in parts {
        let (key, value) = part.split_once('=').map_or((part, None), |(k, v)| {
            (k.trim(), Some(v.trim().trim_matches('"')))
        });
        if map.insert(key, value).is_some() {
            return Err(Error::Handshake("duplicate compression parameter".into()));
        }
        match key {
            "client_no_context_takeover" | "server_no_context_takeover" if value.is_none() => {}
            "client_max_window_bits" | "server_max_window_bits" => {
                if let Some(v) = value {
                    let n = v
                        .parse::<u8>()
                        .map_err(|_| Error::Handshake("invalid compression window".into()))?;
                    if !(8..=15).contains(&n) {
                        return Err(Error::Handshake("invalid compression window".into()));
                    }
                } else if key == "server_max_window_bits" {
                    return Err(Error::Handshake("missing server window".into()));
                }
            }
            _ => return Err(Error::Handshake("unknown compression parameter".into())),
        }
    }
    Ok(map)
}
fn window(map: &BTreeMap<&str, Option<&str>>, key: &str) -> u8 {
    map.get(key)
        .and_then(|v| v.and_then(|s| s.parse().ok()))
        .unwrap_or(15)
}
fn preference(parameter: u8) -> (bool, u8) {
    (
        matches!(parameter, 2 | 5 | 6 | 7),
        match parameter {
            3 | 5 | 7 => 9,
            4 | 6 => 15,
            _ => 0,
        },
    )
}
pub(crate) fn offer(parameter: u8) -> String {
    let (reset, window) = preference(parameter);
    let mut value =
        "permessage-deflate; client_no_context_takeover; client_max_window_bits".to_owned();
    if reset {
        value.push_str("; server_no_context_takeover");
    }
    if window > 0 {
        value.push_str(&format!("; server_max_window_bits={window}"));
    }
    if parameter == 7 {
        value.push_str(", permessage-deflate; client_no_context_takeover; client_max_window_bits; server_no_context_takeover, permessage-deflate; client_no_context_takeover; client_max_window_bits");
    }
    value
}
pub(crate) fn accept(offers: &str, parameter: u8) -> Result<Option<(String, Parameters)>> {
    let (reset, wanted) = preference(parameter);
    for offer in offers.split(',') {
        if !offer.trim().starts_with("permessage-deflate") {
            continue;
        }
        let map = parse(offer.trim())?;
        if wanted > 0 && !map.contains_key("client_max_window_bits") && parameter != 7 {
            continue;
        }
        let tx = window(&map, "server_max_window_bits");
        let rx = if wanted > 0 && map.contains_key("client_max_window_bits") {
            wanted.min(window(&map, "client_max_window_bits"))
        } else {
            15
        };
        // The backend supports 9..=15. Decline 8-bit offers instead of silently
        // advertising an unimplemented window.
        if tx < 9 || rx < 9 {
            continue;
        }
        let tx_reset = map.contains_key("server_no_context_takeover");
        let rx_reset = reset;
        let mut response = "permessage-deflate".to_owned();
        if tx_reset {
            response.push_str("; server_no_context_takeover");
        }
        if rx_reset {
            response.push_str("; client_no_context_takeover");
        }
        if map.contains_key("server_max_window_bits") {
            response.push_str(&format!("; server_max_window_bits={tx}"));
        }
        if wanted > 0 && map.contains_key("client_max_window_bits") {
            response.push_str(&format!("; client_max_window_bits={rx}"));
        }
        return Ok(Some((
            response,
            Parameters {
                tx_window: tx,
                rx_window: rx,
                tx_no_context: tx_reset,
                rx_no_context: rx_reset,
            },
        )));
    }
    Ok(None)
}
pub(crate) fn response(value: &str, parameter: u8) -> Result<Parameters> {
    let map = parse(value)?;
    if map.get("client_max_window_bits") == Some(&None) {
        return Err(Error::Handshake("missing client window".into()));
    }
    let p = Parameters {
        tx_window: window(&map, "client_max_window_bits"),
        rx_window: window(&map, "server_max_window_bits"),
        tx_no_context: map.contains_key("client_no_context_takeover"),
        rx_no_context: map.contains_key("server_no_context_takeover"),
    };
    let (reset, wanted) = preference(parameter);
    if p.tx_window < 9
        || p.rx_window < 9
        || (wanted > 0 && p.rx_window > wanted)
        || (reset && !p.rx_no_context)
    {
        return Err(Error::Handshake(
            "compression response does not satisfy offer".into(),
        ));
    }
    Ok(p)
}

/// Stateful permessage-deflate compressor and decompressor.
pub struct Deflate {
    compressor: Compress,
    decompressor: Decompress,
    parameters: Parameters,
}
impl Deflate {
    /// Construct with validated window sizes. Unsupported values return an error.
    pub fn new(parameters: Parameters) -> Result<Self> {
        if !(9..=15).contains(&parameters.tx_window) || !(9..=15).contains(&parameters.rx_window) {
            return Err(Error::Config("compression windows must be 9..=15".into()));
        }
        Ok(Self {
            compressor: Compress::new_with_window_bits(
                // Autobahn 0.10.9 uses zlib's default level (6). A lower level
                // changes fragmentation workloads and invalidates comparisons.
                Compression::default(),
                false,
                parameters.tx_window,
            ),
            decompressor: Decompress::new_with_window_bits(false, parameters.rx_window),
            parameters,
        })
    }
    /// Compress one message and remove the RFC 7692 sync-flush suffix.
    pub fn encode(&mut self, input: &[u8]) -> Result<Bytes> {
        if self.parameters.tx_no_context {
            self.compressor.reset();
        }
        let mut output = Vec::with_capacity(input.len() / 2 + 128);
        let mut offset = 0;
        loop {
            let mut chunk = [0; 8192];
            let before_in = self.compressor.total_in();
            let before_out = self.compressor.total_out();
            self.compressor
                .compress(&input[offset..], &mut chunk, FlushCompress::Sync)
                .map_err(|e| Error::Compression(e.to_string()))?;
            let used = (self.compressor.total_in() - before_in) as usize;
            let made = (self.compressor.total_out() - before_out) as usize;
            offset += used;
            output.extend_from_slice(&chunk[..made]);
            if offset == input.len() && made < chunk.len() {
                break;
            }
            if made == 0 && used == 0 {
                return Err(Error::Compression("compressor stalled".into()));
            }
        }
        if !output.ends_with(&[0, 0, 255, 255]) {
            return Err(Error::Compression("missing sync flush suffix".into()));
        }
        output.truncate(output.len() - 4);
        Ok(Bytes::from(output))
    }
    /// Decompress one complete message with a hard output bound.
    pub fn decode(&mut self, input: &[u8], limit: usize) -> Result<Bytes> {
        if self.parameters.rx_no_context {
            self.decompressor.reset(false);
        }
        let mut output = Vec::new();
        for data in [input, &[0, 0, 255, 255]] {
            let mut offset = 0;
            loop {
                let mut chunk = [0; 8192];
                let before_in = self.decompressor.total_in();
                let before_out = self.decompressor.total_out();
                self.decompressor
                    .decompress(&data[offset..], &mut chunk, FlushDecompress::Sync)
                    .map_err(|e| Error::Compression(e.to_string()))?;
                let used = (self.decompressor.total_in() - before_in) as usize;
                let made = (self.decompressor.total_out() - before_out) as usize;
                if output.len().saturating_add(made) > limit {
                    return Err(Error::Limit("inflated message"));
                }
                offset += used;
                output.extend_from_slice(&chunk[..made]);
                if offset == data.len() && made < chunk.len() {
                    break;
                }
                if used == 0 && made == 0 {
                    return Err(Error::Compression("decompressor stalled".into()));
                }
            }
        }
        Ok(Bytes::from(output))
    }
}

pub(crate) struct Messages {
    opcode: Option<u8>,
    compressed: bool,
    buf: BytesMut,
    utf8_checked: usize,
    limit: usize,
    pub deflate: Option<Deflate>,
}
impl Messages {
    pub fn new(limit: usize, parameters: Option<Parameters>) -> Result<Self> {
        Ok(Self {
            opcode: None,
            compressed: false,
            buf: BytesMut::new(),
            utf8_checked: 0,
            limit,
            deflate: parameters.map(Deflate::new).transpose()?,
        })
    }
    pub fn push(&mut self, frame: Frame) -> Result<Option<(u8, Bytes)>> {
        if frame.opcode >= 8 {
            return Err(Error::Protocol(
                "control frame passed to message assembler".into(),
            ));
        }
        if frame.rsv & 3 != 0 {
            return Err(Error::Protocol("RSV2/3 set".into()));
        }
        if frame.opcode != 0 {
            if self.opcode.is_some() {
                return Err(Error::Protocol("interleaved data messages".into()));
            }
            self.opcode = Some(frame.opcode);
            self.compressed = frame.rsv == 4;
            self.utf8_checked = 0;
            if self.compressed && self.deflate.is_none() {
                return Err(Error::Protocol("unnegotiated compression".into()));
            }
        } else if self.opcode.is_none() || frame.rsv != 0 {
            return Err(Error::Protocol("invalid continuation".into()));
        }
        if self.buf.len().saturating_add(frame.payload.len()) > self.limit {
            return Err(Error::Limit("message payload"));
        }
        let opcode = self
            .opcode
            .ok_or_else(|| Error::Protocol("missing message opcode".into()))?;
        if self.buf.is_empty() && frame.fin {
            self.opcode = None;
            let data = if self.compressed {
                self.inflate(&frame.payload)?
            } else {
                frame.payload
            };
            if opcode == 1 {
                validate_utf8(&data, true)?;
            }
            return Ok(Some((opcode, data)));
        }
        self.buf.extend_from_slice(&frame.payload);
        if opcode == 1 && !self.compressed {
            self.utf8_checked += validate_utf8(&self.buf[self.utf8_checked..], frame.fin)?;
        }
        if !frame.fin {
            return Ok(None);
        }
        self.opcode = None;
        let wire = self.buf.split().freeze();
        let data = if self.compressed {
            self.inflate(&wire)?
        } else {
            wire
        };
        if opcode == 1 && self.compressed {
            validate_utf8(&data, true)?;
        }
        Ok(Some((opcode, data)))
    }
    fn inflate(&mut self, data: &[u8]) -> Result<Bytes> {
        self.deflate
            .as_mut()
            .ok_or_else(|| Error::Protocol("missing decompressor".into()))?
            .decode(data, self.limit)
    }
}
fn validate_utf8(data: &[u8], fin: bool) -> Result<usize> {
    match std::str::from_utf8(data) {
        Ok(_) => Ok(data.len()),
        Err(e) if e.error_len().is_none() && !fin => Ok(e.valid_up_to()),
        Err(_) => Err(Error::Protocol("invalid UTF-8 text".into())),
    }
}
