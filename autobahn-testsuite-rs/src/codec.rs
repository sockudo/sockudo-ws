//! Allocation-conscious raw framing. Outbound frames deliberately permit invalid inputs.
use crate::{Error, Result};
use bytes::{Buf, Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufWriter};

/// A raw WebSocket frame, without message reassembly.
#[derive(Debug, Clone)]
pub struct Frame {
    /// FIN flag.
    pub fin: bool,
    /// Three RSV bits in wire order (RSV1 = 4).
    pub rsv: u8,
    /// Four-bit opcode, including reserved values.
    pub opcode: u8,
    /// Unmasked payload.
    pub payload: Bytes,
}
impl Frame {
    /// Construct an unfragmented frame with no reserved bits.
    pub fn new(opcode: u8, payload: impl Into<Bytes>) -> Self {
        Self {
            fin: true,
            rsv: 0,
            opcode,
            payload: payload.into(),
        }
    }
}

/// XOR a payload in place; `offset` preserves masking across streamed chunks.
pub fn apply_mask(data: &mut [u8], key: [u8; 4], offset: usize) {
    let mask = std::array::from_fn::<_, 8, _>(|i| key[(i + offset) & 3]);
    let word = u64::from_ne_bytes(mask);
    let (chunks, remainder) = data.as_chunks_mut::<8>();
    for chunk in chunks {
        *chunk = (u64::from_ne_bytes(*chunk) ^ word).to_ne_bytes();
    }
    for (i, byte) in remainder.iter_mut().enumerate() {
        *byte ^= mask[i];
    }
}

/// Encode a raw header into caller-owned storage, returning its length.
/// Allows invalid opcode/RSV combinations for conformance probes.
pub fn encode_header(
    out: &mut [u8; 14],
    fin: bool,
    rsv: u8,
    opcode: u8,
    len: u64,
    mask: Option<[u8; 4]>,
) -> usize {
    out[0] = (u8::from(fin) << 7) | ((rsv & 7) << 4) | (opcode & 15);
    let mut n = if len < 126 {
        out[1] = len as u8;
        2
    } else if len <= 65535 {
        out[1] = 126;
        out[2..4].copy_from_slice(&(len as u16).to_be_bytes());
        4
    } else {
        out[1] = 127;
        out[2..10].copy_from_slice(&len.to_be_bytes());
        10
    };
    if let Some(key) = mask {
        out[1] |= 128;
        out[n..n + 4].copy_from_slice(&key);
        n += 4;
    }
    n
}

/// Decode one complete frame, leaving partial data untouched and checking bounds
/// before allocating. `expect_mask` is true when receiving from a client.
pub fn decode_frame(
    buf: &mut BytesMut,
    expect_mask: bool,
    max_frame: usize,
) -> Result<Option<Frame>> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let (fin, rsv, opcode) = (buf[0] & 128 != 0, (buf[0] >> 4) & 7, buf[0] & 15);
    let masked = buf[1] & 128 != 0;
    if masked != expect_mask {
        return Err(Error::Protocol("incorrect masking direction".into()));
    }
    let short = buf[1] & 127;
    let n = match short {
        126 => 4,
        127 => 10,
        _ => 2,
    };
    if buf.len() < n {
        return Ok(None);
    }
    let len = match short {
        126 => u16::from_be_bytes([buf[2], buf[3]]) as u64,
        127 => {
            let mut a = [0; 8];
            a.copy_from_slice(&buf[2..10]);
            u64::from_be_bytes(a)
        }
        _ => short as u64,
    };
    if len >> 63 != 0 || (short == 126 && len < 126) || (short == 127 && len <= 65535) {
        return Err(Error::Protocol("noncanonical payload length".into()));
    }
    if !matches!(opcode, 0 | 1 | 2 | 8 | 9 | 10) {
        return Err(Error::Protocol("reserved opcode".into()));
    }
    if opcode >= 8 && (!fin || len > 125 || rsv != 0) {
        return Err(Error::Protocol("invalid control frame".into()));
    }
    if len > max_frame as u64 {
        return Err(Error::Limit("frame payload"));
    }
    let header = n + if masked { 4 } else { 0 };
    let len = len as usize;
    let complete_len = header
        .checked_add(len)
        .ok_or(Error::Limit("frame length overflow"))?;
    if buf.len() < complete_len {
        return Ok(None);
    }
    let key = if masked {
        Some([buf[n], buf[n + 1], buf[n + 2], buf[n + 3]])
    } else {
        None
    };
    buf.advance(header);
    let mut data = buf.split_to(len);
    if let Some(key) = key {
        apply_mask(&mut data, key, 0);
    }
    Ok(Some(Frame {
        fin,
        rsv,
        opcode,
        payload: data.freeze(),
    }))
}

pub(crate) struct Reader<R> {
    io: R,
    buf: BytesMut,
    server: bool,
    max_frame: usize,
    pub bytes: u64,
    pub frames: u64,
    checked_prefix: usize,
}
impl<R: AsyncRead + Unpin> Reader<R> {
    pub fn new(io: R, initial: BytesMut, server: bool, max_frame: usize) -> Self {
        Self {
            io,
            buf: initial,
            server,
            max_frame,
            bytes: 0,
            frames: 0,
            checked_prefix: 0,
        }
    }
    // read_buf is cancellation safe; partially received bytes stay in self.buf.
    pub async fn next(&mut self) -> Result<Option<Frame>> {
        loop {
            if let Some(frame) = decode_frame(&mut self.buf, self.server, self.max_frame)? {
                self.frames += 1;
                self.checked_prefix = 0;
                return Ok(Some(frame));
            }
            self.check_text_prefix()?;
            self.buf.reserve(8192);
            let n = self.io.read_buf(&mut self.buf).await?;
            self.bytes += n as u64;
            if n == 0 {
                return if self.buf.is_empty() {
                    Ok(None)
                } else {
                    Err(Error::Protocol("truncated frame".into()))
                };
            }
        }
    }
    // Inspect only newly arrived text bytes, retaining an incomplete code point.
    // This catches invalid UTF-8 before a deliberately unfinished frame ends.
    fn check_text_prefix(&mut self) -> Result<()> {
        if self.buf.len() < 2 || self.buf[0] & 0x7f != 1 {
            return Ok(());
        }
        let n = match self.buf[1] & 127 {
            126 => 4,
            127 => 10,
            _ => 2,
        };
        let masked = self.buf[1] & 128 != 0;
        let header = n + if masked { 4 } else { 0 };
        if self.buf.len() < header {
            return Ok(());
        }
        let data = &self.buf[header..];
        while self.checked_prefix < data.len() {
            let start = self.checked_prefix;
            let end = (start + 8192).min(data.len());
            let mut scratch = [0; 8192];
            let slice = if masked {
                scratch[..end - start].copy_from_slice(&data[start..end]);
                apply_mask(
                    &mut scratch[..end - start],
                    [
                        self.buf[n],
                        self.buf[n + 1],
                        self.buf[n + 2],
                        self.buf[n + 3],
                    ],
                    start,
                );
                &scratch[..end - start]
            } else {
                &data[start..end]
            };
            match std::str::from_utf8(slice) {
                Ok(_) => self.checked_prefix = end,
                Err(error) if error.error_len().is_none() => {
                    self.checked_prefix += error.valid_up_to();
                    if end == data.len() {
                        break;
                    }
                }
                Err(_) => return Err(Error::Protocol("invalid UTF-8 text prefix".into())),
            }
        }
        Ok(())
    }
}

pub(crate) struct Writer<W> {
    io: BufWriter<W>,
    client: bool,
    scratch: Vec<u8>,
    stream_mask: Option<[u8; 4]>,
    stream_offset: usize,
    pub bytes: u64,
    pub frames: u64,
}
impl<W: AsyncWrite + Unpin> Writer<W> {
    pub fn new(io: W, client: bool) -> Self {
        Self {
            io: BufWriter::with_capacity(65536, io),
            client,
            scratch: Vec::with_capacity(65536),
            stream_mask: None,
            stream_offset: 0,
            bytes: 0,
            frames: 0,
        }
    }
    pub async fn header(&mut self, fin: bool, rsv: u8, opcode: u8, len: usize) -> Result<()> {
        self.stream_mask = if self.client {
            Some(rand::random())
        } else {
            None
        };
        self.stream_offset = 0;
        let mut header = [0; 14];
        let n = encode_header(&mut header, fin, rsv, opcode, len as u64, self.stream_mask);
        self.io.write_all(&header[..n]).await?;
        self.bytes += n as u64;
        self.frames += 1;
        Ok(())
    }
    pub async fn data(&mut self, data: &[u8], chop: usize) -> Result<()> {
        let width = if chop == 0 { 65536 } else { chop.min(65536) };
        for chunk in data.chunks(width) {
            if let Some(key) = self.stream_mask {
                self.scratch.clear();
                self.scratch.extend_from_slice(chunk);
                apply_mask(&mut self.scratch, key, self.stream_offset);
                self.io.write_all(&self.scratch).await?;
            } else {
                self.io.write_all(chunk).await?;
            }
            self.stream_offset += chunk.len();
            self.bytes += chunk.len() as u64;
            if chop > 0 {
                self.io.flush().await?;
                tokio::task::yield_now().await;
            }
        }
        Ok(())
    }
    pub async fn frame(&mut self, frame: &Frame, chop: usize) -> Result<()> {
        if chop > 0 {
            // Chop boundaries apply to the entire wire frame, including its header.
            let mask = if self.client {
                Some(rand::random())
            } else {
                None
            };
            let mut header = [0; 14];
            let n = encode_header(
                &mut header,
                frame.fin,
                frame.rsv,
                frame.opcode,
                frame.payload.len() as u64,
                mask,
            );
            let mut wire = Vec::with_capacity(n + frame.payload.len());
            wire.extend_from_slice(&header[..n]);
            wire.extend_from_slice(&frame.payload);
            if let Some(key) = mask {
                apply_mask(&mut wire[n..], key, 0);
            }
            for chunk in wire.chunks(chop) {
                self.io.write_all(chunk).await?;
                self.io.flush().await?;
                tokio::task::yield_now().await;
            }
            self.frames += 1;
            self.bytes += wire.len() as u64;
            Ok(())
        } else {
            self.header(frame.fin, frame.rsv, frame.opcode, frame.payload.len())
                .await?;
            self.data(&frame.payload, 0).await
        }
    }
    pub async fn message(
        &mut self,
        opcode: u8,
        data: Bytes,
        fragment: usize,
        rsv: u8,
    ) -> Result<()> {
        if fragment == 0 || data.len() <= fragment {
            return self
                .frame(
                    &Frame {
                        fin: true,
                        rsv,
                        opcode,
                        payload: data,
                    },
                    0,
                )
                .await;
        }
        // The pinned sender terminates exact multiples with an empty final
        // continuation. Preserve that boundary case as part of the workload.
        for start in (0..=data.len()).step_by(fragment) {
            let end = (start + fragment).min(data.len());
            self.frame(
                &Frame {
                    fin: end - start < fragment,
                    rsv: if start == 0 { rsv } else { 0 },
                    opcode: if start == 0 { opcode } else { 0 },
                    payload: data.slice(start..end),
                },
                0,
            )
            .await?;
        }
        Ok(())
    }
    pub async fn flush(&mut self) -> Result<()> {
        self.io.flush().await?;
        Ok(())
    }
    pub async fn shutdown(&mut self) -> Result<()> {
        self.io.shutdown().await?;
        Ok(())
    }
}

pub(crate) fn close_code(payload: &[u8]) -> Result<Option<u16>> {
    if payload.is_empty() {
        return Ok(None);
    }
    if payload.len() == 1 {
        return Err(Error::Protocol("close payload has one byte".into()));
    }
    let code = u16::from_be_bytes([payload[0], payload[1]]);
    if !(matches!(code,1000..=1003|1007..=1014|3000..=4999)) {
        return Err(Error::Protocol("invalid close code".into()));
    }
    std::str::from_utf8(&payload[2..])
        .map_err(|_| Error::Protocol("invalid UTF-8 close reason".into()))?;
    Ok(Some(code))
}
