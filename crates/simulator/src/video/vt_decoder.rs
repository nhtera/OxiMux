//! Hardware H.264 decoding with VideoToolbox, straight into the pixel buffers
//! GPUI paints with `surface()` — no copy, no conversion on the CPU.
//!
//! GPUI's Metal renderer takes a `core_video` (0.5) `CVPixelBuffer`, asserts
//! it is full-range NV12 and unwraps the textures it makes from its two
//! planes. So the session is asked for exactly that (IOSurface-backed,
//! Metal-compatible `420f`), and every decoded buffer is checked again before
//! it leaves here: a frame that does not conform is dropped with a warning,
//! because a bad one would panic the app, not just look wrong.

use std::ffi::c_void;
use std::ptr::{self, NonNull};

use core_foundation::base::TCFType;
use core_video::pixel_buffer::{CVPixelBuffer, CVPixelBufferRef, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, kCFAllocatorNull};
use objc2_core_media::{CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMTime, CMVideoFormatDescriptionCreateFromH264ParameterSets};
use objc2_core_video::{
    CVImageBuffer, kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey, kCVPixelBufferPixelFormatTypeKey,
};
use objc2_video_toolbox::{VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord, VTDecompressionSession};

use super::annexb::{self, ParameterSets};
use crate::{Result, SimError};

type OnFrame = Box<dyn Fn(Picture) + Send + Sync>;

/// A decoded, paintable picture, movable to the thread that paints it.
///
/// `core_video`'s type is not `Send` only because it is a raw pointer; a
/// CoreVideo buffer is a CoreFoundation object whose retain and release are
/// thread-safe, and nothing here writes to its pixels after decoding.
#[derive(Clone)]
pub struct Picture(CVPixelBuffer);

// SAFETY: see the type's doc — ownership moves, the pixels are never written.
unsafe impl Send for Picture {}
unsafe impl Sync for Picture {}

impl Picture {
    pub fn buffer(&self) -> &CVPixelBuffer {
        &self.0
    }

    pub fn into_buffer(self) -> CVPixelBuffer {
        self.0
    }

    /// Width and height in pixels.
    pub fn size(&self) -> (usize, usize) {
        (self.0.get_width(), self.0.get_height())
    }
}

impl std::fmt::Debug for Picture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Picture({}x{})", self.0.get_width(), self.0.get_height())
    }
}

/// One decompression session, for one set of parameter sets (a new capture
/// session — a rotation — may bring new ones: make a new decoder then).
pub struct Decoder {
    session: CFRetained<VTDecompressionSession>,
    format: CFRetained<CMFormatDescription>,
    params: ParameterSets,
    /// Owned here; the output callback borrows it through its refcon.
    on_frame: *mut OnFrame,
}

// The session is thread-safe for our use (one decoding thread at a time), and
// `on_frame` is `Send + Sync`.
unsafe impl Send for Decoder {}

impl Decoder {
    /// A session for the stream `params` describe. `on_frame` gets every
    /// decoded picture (on the decoding thread).
    pub fn new(params: &ParameterSets, on_frame: impl Fn(Picture) + Send + Sync + 'static) -> Result<Self> {
        let format = format_description(params)?;
        let attributes = destination_attributes();
        let on_frame: *mut OnFrame = Box::into_raw(Box::new(Box::new(on_frame)));
        let callback = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(output_callback),
            decompressionOutputRefCon: on_frame.cast(),
        };
        let mut session: *mut VTDecompressionSession = ptr::null_mut();
        // SAFETY: every pointer is valid for the call; the callback's refcon
        // outlives the session (freed in `Drop` after invalidating it).
        let status = unsafe {
            VTDecompressionSession::create(
                None,
                &format,
                None,
                Some(attributes.as_opaque()),
                &callback,
                NonNull::from(&mut session),
            )
        };
        let Some(session) = NonNull::new(session).filter(|_| status == 0) else {
            // SAFETY: not handed to any session.
            drop(unsafe { Box::from_raw(on_frame) });
            return Err(vt_error("create a decoding session", status));
        };
        // SAFETY: `create` returned it retained.
        let session = unsafe { CFRetained::from_raw(session) };
        Ok(Self { session, format, params: params.clone(), on_frame })
    }

    /// The parameter sets this session decodes.
    pub fn params(&self) -> &ParameterSets {
        &self.params
    }

    /// Decode one picture (an Annex-B media packet). Returns once the frame
    /// callback has run for it (decoding is synchronous).
    pub fn decode(&self, annexb_picture: &[u8]) -> Result<()> {
        let mut sample = annexb::to_avcc(annexb_picture);
        if sample.is_empty() {
            return Ok(());
        }
        let len = sample.len();
        let mut block: *mut CMBlockBuffer = ptr::null_mut();
        // SAFETY: `sample` outlives the block (the decode below is synchronous
        // and waited for); `kCFAllocatorNull` means CoreMedia never frees it.
        let status = unsafe {
            CMBlockBuffer::create_with_memory_block(
                None,
                sample.as_mut_ptr().cast(),
                len,
                kCFAllocatorNull,
                ptr::null(),
                0,
                len,
                0,
                NonNull::from(&mut block),
            )
        };
        let block = NonNull::new(block).filter(|_| status == 0).ok_or_else(|| vt_error("wrap a video sample", status))?;
        // SAFETY: returned retained.
        let block = unsafe { CFRetained::from_raw(block) };
        let mut buffer: *mut CMSampleBuffer = ptr::null_mut();
        let sizes = [len];
        // SAFETY: valid pointers; one sample, no timing (we decode in order).
        let status = unsafe {
            CMSampleBuffer::create_ready(None, Some(&block), Some(&self.format), 1, 0, ptr::null(), 1, sizes.as_ptr(), NonNull::from(&mut buffer))
        };
        let buffer = NonNull::new(buffer).filter(|_| status == 0).ok_or_else(|| vt_error("make a video sample", status))?;
        // SAFETY: returned retained.
        let buffer = unsafe { CFRetained::from_raw(buffer) };
        let mut info = VTDecodeInfoFlags::empty();
        // SAFETY: synchronous decode (no asynchronous flag); waited for below.
        let status = unsafe { self.session.decode_frame(&buffer, VTDecodeFrameFlags::empty(), ptr::null_mut(), &mut info) };
        // SAFETY: the session is valid.
        unsafe { self.session.wait_for_asynchronous_frames() };
        // The CoreMedia objects point into `sample`: release them first.
        drop(buffer);
        drop(block);
        drop(sample);
        if status != 0 {
            return Err(vt_error("decode a frame", status));
        }
        Ok(())
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: after invalidating, the session calls back no more, so the
        // refcon can be freed.
        unsafe {
            self.session.wait_for_asynchronous_frames();
            self.session.invalidate();
            drop(Box::from_raw(self.on_frame));
        }
    }
}

/// VideoToolbox's output callback: validate, bridge to `core_video`, hand on.
unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    _source_frame_refcon: *mut c_void,
    status: i32,
    _flags: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    _pts: CMTime,
    _duration: CMTime,
) {
    if status != 0 || image.is_null() || refcon.is_null() {
        if status != 0 {
            tracing::debug!(status, "a video frame did not decode");
        }
        return;
    }
    // SAFETY: VideoToolbox passes a valid image buffer for the duration of the
    // call; "get rule" retains it for us.
    let buffer = unsafe { CVPixelBuffer::wrap_under_get_rule(image as CVPixelBufferRef) };
    if !is_paintable(&buffer) {
        tracing::warn!(format = buffer.get_pixel_format(), "dropped a decoded frame GPUI cannot paint");
        return;
    }
    // SAFETY: the refcon is the decoder's `on_frame`, alive until after the
    // session is invalidated.
    let on_frame = unsafe { &*(refcon as *const OnFrame) };
    on_frame(Picture(buffer));
}

/// What GPUI's `surface()` needs: full-range NV12, two non-empty planes.
pub fn is_paintable(buffer: &CVPixelBuffer) -> bool {
    // SAFETY: a plain getter on a valid, retained buffer.
    let io_surface = unsafe { CVPixelBufferGetIOSurface(buffer.as_concrete_TypeRef()) };
    // GPUI makes Metal textures from the IOSurface and unwraps them.
    !io_surface.is_null()
        && buffer.get_pixel_format() == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
        && buffer.is_planar()
        && buffer.get_plane_count() == 2
        && buffer.get_width() > 0
        && buffer.get_height() > 0
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferGetIOSurface(buffer: CVPixelBufferRef) -> *const c_void;
}

fn format_description(params: &ParameterSets) -> Result<CFRetained<CMFormatDescription>> {
    if params.sps.is_empty() || params.pps.is_empty() {
        return Err(SimError::Protocol("empty H.264 parameter sets".into()));
    }
    let pointers = [NonNull::from(&params.sps[0]), NonNull::from(&params.pps[0])];
    let sizes = [params.sps.len(), params.pps.len()];
    let mut format: *const CMFormatDescription = ptr::null();
    // SAFETY: two parameter sets, pointers and sizes valid for the call.
    let status = unsafe {
        CMVideoFormatDescriptionCreateFromH264ParameterSets(
            None,
            2,
            NonNull::from(&pointers[0]).cast(),
            NonNull::from(&sizes[0]),
            4,
            NonNull::from(&mut format),
        )
    };
    let format = NonNull::new(format as *mut CMFormatDescription)
        .filter(|_| status == 0)
        .ok_or_else(|| vt_error("read the stream's parameter sets", status))?;
    // SAFETY: returned retained.
    Ok(unsafe { CFRetained::from_raw(format) })
}

/// IOSurface-backed, Metal-compatible full-range NV12.
fn destination_attributes() -> CFRetained<CFDictionary<CFString, CFType>> {
    let format = CFNumber::new_i32(kCVPixelFormatType_420YpCbCr8BiPlanarFullRange as i32);
    let io_surface: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(&[], &[]);
    // SAFETY: the keys are CoreVideo's own constants.
    let keys: [&CFString; 3] =
        unsafe { [kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey] };
    let values: [&CFType; 3] = [format.as_ref(), io_surface.as_ref(), CFBoolean::new(true).as_ref()];
    CFDictionary::from_slices(&keys, &values)
}

fn vt_error(what: &str, status: i32) -> SimError {
    SimError::HelperFailed(format!("could not {what} (VideoToolbox status {status})"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const CONFIG: &[u8] = include_bytes!("../../tests/fixtures/android-160/config.h264");
    const KEY: &[u8] = include_bytes!("../../tests/fixtures/android-160/key.h264");

    /// A real key frame captured from an Android 17 emulator (scrcpy 4.1,
    /// `max_size=160`) decodes to one paintable 72×160 NV12 buffer.
    #[test]
    fn a_captured_key_frame_decodes_to_a_paintable_buffer() {
        let params = annexb::parameter_sets(CONFIG).expect("SPS and PPS");
        let frames = Arc::new(Mutex::new(Vec::new()));
        let sink = frames.clone();
        let decoder = Decoder::new(&params, move |buf| sink.lock().unwrap().push(buf)).expect("a session");
        decoder.decode(KEY).expect("decoded");
        let frames = frames.lock().unwrap();
        assert_eq!(frames.len(), 1);
        let frame = &frames[0];
        assert!(is_paintable(frame.buffer()));
        assert_eq!(frame.size(), (72, 160));
        assert_eq!(decoder.params(), &params);
    }

    #[test]
    fn garbage_parameter_sets_are_an_error_not_a_crash() {
        let bogus = ParameterSets { sps: vec![0x67, 0xff], pps: vec![0x68] };
        assert!(Decoder::new(&bogus, |_| {}).is_err());
    }
}
