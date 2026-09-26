//! H.264 video between the device and the decoder (P10): splitting what
//! `MediaCodec` writes (Annex-B) into what VideoToolbox reads (parameter sets
//! plus length-prefixed AVCC samples), and decoding it on the GPU.

pub mod annexb;
#[cfg(target_os = "macos")]
pub mod vt_decoder;
