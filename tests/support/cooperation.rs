use bytes::{Bytes, BytesMut};
use sockudo_ws::OpCode;
use sockudo_ws::frame::encode_frame_with_rsv;

pub fn payload(sequence: usize) -> [u8; 64] {
    [sequence as u8; 64]
}

pub fn buffered_frames(compressed: bool) -> Bytes {
    let mut wire = BytesMut::new();
    #[cfg(feature = "permessage-deflate")]
    let mut encoder =
        sockudo_ws::deflate::DeflateEncoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, true, 1, 0);
    for sequence in 0..256 {
        let original = payload(sequence);
        #[cfg(feature = "permessage-deflate")]
        let encoded = compressed.then(|| encoder.compress(&original).unwrap().unwrap());
        #[cfg(not(feature = "permessage-deflate"))]
        let encoded: Option<Bytes> = {
            assert!(!compressed);
            None
        };
        encode_frame_with_rsv(
            &mut wire,
            OpCode::Binary,
            encoded.as_deref().unwrap_or(&original),
            true,
            None,
            compressed,
        );
    }
    wire.freeze()
}
