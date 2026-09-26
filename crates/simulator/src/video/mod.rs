//! H.264 video between the device and the decoder: splitting what Android's
//! `MediaCodec` writes (Annex-B) into what VideoToolbox reads (parameter sets
//! plus length-prefixed AVCC samples), reading the iOS helper's avcC records,
//! and decoding it on the GPU.

pub mod annexb;
#[cfg(target_os = "macos")]
pub mod vt_decoder;
