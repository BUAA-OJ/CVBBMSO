//! `VideoToolbox` H.264 encoding.

use std::{
    ffi::c_void,
    io,
    marker::PhantomData,
    ptr::{self, NonNull},
    sync::{Arc, Condvar, Mutex, PoisonError},
    time::{Duration, Instant},
};

use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMFormatDescription, CMSampleBuffer, CMTime,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex, kCMTimeInvalid, kCMVideoCodecType_H264,
};
use objc2_core_video::{
    CVBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty,
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTProfileLevel_H264_High_AutoLevel,
};

use super::Nv12Frame;
use crate::RecordingError;
use crate::container::{AvcConfig, ContainerError, Sample};

/// Default bitrate for a desktop recording, in bits per second.
pub(crate) const DEFAULT_BITRATE: u32 = 4_000_000;

/// Timescale used for presentation timestamps, in units per second.
const TIMESCALE: i32 = 1_000_000;
/// Byte length size of the AVCC framing `VideoToolbox` emits.
const NAL_LENGTH_SIZE: i32 = 4;
/// Bound on how long a flush waits for the encoder to drain.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Failure inside the `VideoToolbox` adapter.
#[derive(Debug, thiserror::Error)]
enum EncoderError {
    /// A `VideoToolbox` call returned a non-zero status.
    #[error("VideoToolbox status {status} while {operation}")]
    Status {
        /// The raw `OSStatus` value.
        status: i32,
        /// What the adapter was doing.
        operation: &'static str,
    },
    /// The encoder returned no session object.
    #[error("VideoToolbox returned no compression session")]
    NoSession,
    /// A pending frame did not complete in time.
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),
    /// The encoder output omitted a required parameter set.
    #[error("encoder output is missing the {0}")]
    MissingParameterSet(&'static str),
    /// The encoder used a NAL length size the containers cannot store.
    #[error("encoder output uses {0}-byte NAL lengths instead of {NAL_LENGTH_SIZE}")]
    UnsupportedLengthSize(i32),
    /// The frame does not fit the encoder's bounds.
    #[error("the frame does not fit the encoder")]
    UnsupportedFrame,
    /// The output callback could not parse a sample.
    #[error("the encoder output callback failed")]
    Callback(#[source] io::Error),
    /// The encoder emitted data that cannot form a valid AVC configuration.
    #[error("invalid encoder output: {0}")]
    Container(
        #[from]
        #[source]
        ContainerError,
    ),
}

type EncoderResult<T> = Result<T, EncoderError>;

/// Progress of the asynchronous `VideoToolbox` callback.
#[derive(Default)]
struct State {
    /// Samples the callback has produced and not yet handed to the caller.
    samples: Vec<Sample>,
    /// Parameter sets observed in the first output format description.
    config: Option<AvcConfig>,
    /// Frames submitted to the encoder.
    submitted: usize,
    /// Frames the callback has completed, including dropped frames.
    completed: usize,
    /// First callback failure, reported by the next call.
    failure: Option<io::Error>,
}

/// State shared with the output callback.
struct Shared {
    state: Mutex<State>,
    ready: Condvar,
}

/// Configured H.264 encoder fed with NV12 frames.
pub(crate) struct Encoder {
    session: CFRetained<VTCompressionSession>,
    shared: Arc<Shared>,
    width: u32,
    height: u32,
    fps: u32,
    /// Last presentation timestamp, in timescale units.
    last_pts: i64,
    /// Whether the next frame must be an IDR.
    force_keyframe: bool,
    // The session belongs to the thread that created it.
    _thread: PhantomData<*const ()>,
}

impl Encoder {
    /// Opens an H.264 encoder for `width` by `height` frames at `fps`.
    ///
    /// The encoder primes one frame during construction so that
    /// [`Encoder::config`] is available before the first real frame; that frame
    /// is encoded with the same settings and discarded.
    ///
    /// # Errors
    ///
    /// Returns [`RecordingError::Encoder`] when `VideoToolbox` cannot create or
    /// configure the session, or when it produces no parameter sets.
    pub(crate) fn open(
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
    ) -> Result<Self, RecordingError> {
        Self::open_inner(width, height, fps, bitrate).map_err(encoder_failure)
    }

    fn open_inner(width: u32, height: u32, fps: u32, bitrate: u32) -> EncoderResult<Self> {
        if width == 0 || height == 0 || fps == 0 {
            return Err(EncoderError::UnsupportedFrame);
        }
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            ready: Condvar::new(),
        });
        let attributes = source_attributes(width, height)?;
        let mut raw: *mut VTCompressionSession = ptr::null_mut();
        // SAFETY: `shared` outlives the session because the returned encoder
        // owns both and releases the session first. All out pointers are valid
        // for the duration of the call.
        let status = unsafe {
            VTCompressionSession::create(
                None,
                i32::try_from(width).map_err(|_| EncoderError::UnsupportedFrame)?,
                i32::try_from(height).map_err(|_| EncoderError::UnsupportedFrame)?,
                kCMVideoCodecType_H264,
                None,
                Some(attributes.as_opaque()),
                None,
                Some(output_callback),
                Arc::as_ptr(&shared).cast_mut().cast(),
                NonNull::from(&mut raw),
            )
        };
        if status != 0 {
            return Err(EncoderError::Status {
                status,
                operation: "creating the compression session",
            });
        }
        let Some(raw) = NonNull::new(raw) else {
            return Err(EncoderError::NoSession);
        };
        // SAFETY: a successful creation returns one owned session reference.
        let session = unsafe { CFRetained::from_raw(raw) };
        let mut encoder = Self {
            session,
            shared,
            width,
            height,
            fps,
            last_pts: 0,
            force_keyframe: true,
            _thread: PhantomData,
        };
        encoder.configure(bitrate)?;
        encoder.prime()?;
        Ok(encoder)
    }

    /// Encodes one NV12 frame and returns the samples produced so far.
    ///
    /// `VideoToolbox` encodes asynchronously, so a frame's sample can arrive on a
    /// later call. Samples always keep their encoder order; [`Encoder::flush`]
    /// waits for the remaining ones.
    ///
    /// # Errors
    ///
    /// Returns [`RecordingError::Encoder`] when the frame is rejected or the
    /// output callback reported malformed data.
    pub(crate) fn encode(
        &mut self,
        frame: &Nv12Frame,
        timestamp: Duration,
    ) -> Result<Vec<Sample>, RecordingError> {
        self.encode_inner(frame, timestamp).map_err(encoder_failure)
    }

    /// Waits for the frames the encoder still holds after the last input.
    ///
    /// # Errors
    ///
    /// Returns [`RecordingError::Encoder`] when the encoder cannot complete its
    /// pending frames in time.
    pub(crate) fn flush(&mut self) -> Result<Vec<Sample>, RecordingError> {
        self.flush_inner().map_err(encoder_failure)
    }

    /// Requests that the next encoded frame be a keyframe.
    ///
    /// The recorder uses this when rotating to a new segment so the segment can
    /// be decoded independently of the previous one.
    #[allow(clippy::unnecessary_wraps)] // The platform-neutral API reports failures.
    pub(crate) fn force_keyframe(&mut self) -> Result<(), RecordingError> {
        self.force_keyframe = true;
        Ok(())
    }

    /// Returns the AVC configuration built from the primed format description.
    ///
    /// # Errors
    ///
    /// Returns [`RecordingError::Encoder`] when no parameter sets were observed.
    pub(crate) fn config(&self) -> Result<AvcConfig, RecordingError> {
        self.cached_config().map_err(encoder_failure)
    }

    fn encode_inner(
        &mut self,
        frame: &Nv12Frame,
        timestamp: Duration,
    ) -> EncoderResult<Vec<Sample>> {
        let pixel_buffer = self.pixel_buffer(frame)?;
        let presentation = self.presentation_time(timestamp);
        let duration =
            unsafe { CMTime::new(i64::from(TIMESCALE) / i64::from(self.fps.max(1)), TIMESCALE) };
        let properties = if self.force_keyframe {
            Some(force_keyframe_properties())
        } else {
            None
        };
        self.reserve_submission()?;
        let status = unsafe {
            self.session.encode_frame(
                &pixel_buffer,
                presentation,
                duration,
                properties.as_deref().map(CFDictionary::as_opaque),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if status != 0 {
            self.cancel_submission();
            return Err(EncoderError::Status {
                status,
                operation: "encoding a frame",
            });
        }
        self.force_keyframe = false;
        self.take_samples()
    }

    fn flush_inner(&mut self) -> EncoderResult<Vec<Sample>> {
        // SAFETY: the session is live; completing all frames is explicit and
        // only affects frames already submitted.
        let status = unsafe { self.session.complete_frames(kCMTimeInvalid) };
        if status != 0 {
            return Err(EncoderError::Status {
                status,
                operation: "completing pending frames",
            });
        }
        self.wait_for_completion(DRAIN_TIMEOUT)?;
        self.take_samples()
    }

    fn configure(&mut self, bitrate: u32) -> EncoderResult<()> {
        // SAFETY: the session is live and the property values are Core
        // Foundation objects with static or local lifetimes that outlive the call.
        unsafe {
            set_property(
                &self.session,
                kVTCompressionPropertyKey_RealTime,
                CFBoolean::new(true),
            )?;
            set_property(
                &self.session,
                kVTCompressionPropertyKey_AllowFrameReordering,
                CFBoolean::new(false),
            )?;
            set_property(
                &self.session,
                kVTCompressionPropertyKey_ProfileLevel,
                kVTProfileLevel_H264_High_AutoLevel,
            )?;
        }
        let bitrate = CFNumber::new_i32(i32::try_from(bitrate).unwrap_or(i32::MAX));
        let gop = CFNumber::new_i32(i32::try_from(self.fps.saturating_mul(2)).unwrap_or(i32::MAX));
        let fps = CFNumber::new_i32(i32::try_from(self.fps).unwrap_or(i32::MAX));
        // SAFETY: the session is live and the property keys are framework
        // constants; the values outlive the call.
        unsafe {
            set_property(
                &self.session,
                kVTCompressionPropertyKey_AverageBitRate,
                &bitrate,
            )?;
            set_property(
                &self.session,
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                &gop,
            )?;
            set_property(
                &self.session,
                kVTCompressionPropertyKey_ExpectedFrameRate,
                &fps,
            )?;
        }
        Ok(())
    }

    /// Encodes one black frame so the parameter sets exist before the first
    /// real frame. The sample itself is discarded; the next real frame is
    /// forced to an IDR so the stream starts independently decodable.
    fn prime(&mut self) -> EncoderResult<()> {
        let frame =
            Nv12Frame::new(self.width, self.height).map_err(|_| EncoderError::UnsupportedFrame)?;
        let samples = self.encode_inner(&frame, Duration::ZERO)?;
        drop(samples);
        let tail = self.flush_inner()?;
        drop(tail);
        self.cached_config()?;
        self.force_keyframe = true;
        Ok(())
    }

    /// Creates the NV12 pixel buffer and copies the converted frame into it.
    fn pixel_buffer(&self, frame: &Nv12Frame) -> EncoderResult<CFRetained<CVBuffer>> {
        let mut raw: *mut CVBuffer = ptr::null_mut();
        // SAFETY: `raw` is a valid out pointer; the attributes are optional.
        let created = unsafe {
            CVPixelBufferCreate(
                None,
                usize::try_from(self.width).map_err(|_| EncoderError::UnsupportedFrame)?,
                usize::try_from(self.height).map_err(|_| EncoderError::UnsupportedFrame)?,
                kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                None,
                NonNull::from(&mut raw),
            )
        };
        if created != 0 {
            return Err(EncoderError::Status {
                status: created,
                operation: "allocating a pixel buffer",
            });
        }
        let Some(raw) = NonNull::new(raw) else {
            return Err(EncoderError::UnsupportedFrame);
        };
        // SAFETY: a successful `CVPixelBufferCreate` returns one owned reference.
        let buffer = unsafe { CFRetained::from_raw(raw) };
        // SAFETY: the buffer is live and is written only while locked.
        let locked = unsafe { CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags(0)) };
        if locked != 0 {
            return Err(EncoderError::Status {
                status: locked,
                operation: "locking a pixel buffer",
            });
        }
        let copied = self.copy_planes(&buffer, frame);
        // SAFETY: the buffer was locked above and is unlocked exactly once.
        unsafe { CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags(0)) };
        copied?;
        Ok(buffer)
    }

    fn copy_planes(&self, buffer: &CVBuffer, frame: &Nv12Frame) -> EncoderResult<()> {
        let bytes = frame.as_bytes();
        let luma_length = usize::try_from(self.width)
            .ok()
            .and_then(|width| width.checked_mul(usize::try_from(self.height).ok()?))
            .ok_or(EncoderError::UnsupportedFrame)?;
        let (luma, chroma) = bytes
            .split_at_checked(luma_length)
            .ok_or(EncoderError::UnsupportedFrame)?;
        let plane_rows = [
            (
                0_usize,
                luma,
                usize::try_from(self.height)
                    .ok()
                    .ok_or(EncoderError::UnsupportedFrame)?,
            ),
            (
                1_usize,
                chroma,
                usize::try_from(self.height / 2)
                    .ok()
                    .ok_or(EncoderError::UnsupportedFrame)?,
            ),
        ];
        for (index, plane, rows) in plane_rows {
            let base = CVPixelBufferGetBaseAddressOfPlane(buffer, index);
            let stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, index);
            let row_bytes =
                usize::try_from(self.width).map_err(|_| EncoderError::UnsupportedFrame)?;
            if base.is_null() || stride < row_bytes || plane.len() < row_bytes.saturating_mul(rows)
            {
                return Err(EncoderError::UnsupportedFrame);
            }
            for (row, source) in plane.chunks_exact(row_bytes).take(rows).enumerate() {
                // SAFETY: the destination row is inside the locked plane and the
                // source row is exactly `row_bytes` long.
                unsafe {
                    ptr::copy_nonoverlapping(
                        source.as_ptr(),
                        base.cast::<u8>().add(row * stride),
                        row_bytes,
                    );
                }
            }
        }
        Ok(())
    }

    fn reserve_submission(&self) -> EncoderResult<()> {
        let mut state = self.lock_state();
        if let Some(failure) = state.failure.take() {
            return Err(EncoderError::Callback(failure));
        }
        state.submitted = state.submitted.saturating_add(1);
        Ok(())
    }

    fn cancel_submission(&self) {
        let mut state = self.lock_state();
        state.submitted = state.submitted.saturating_sub(1);
    }

    fn take_samples(&self) -> EncoderResult<Vec<Sample>> {
        let mut state = self.lock_state();
        if let Some(failure) = state.failure.take() {
            return Err(EncoderError::Callback(failure));
        }
        Ok(std::mem::take(&mut state.samples))
    }

    fn wait_for_completion(&self, timeout: Duration) -> EncoderResult<()> {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock_state();
        while state.completed < state.submitted {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(EncoderError::Timeout("the encoder to drain"));
            }
            let (guard, wait) = self
                .shared
                .ready
                .wait_timeout(state, remaining)
                .unwrap_or_else(PoisonError::into_inner);
            state = guard;
            if wait.timed_out() && state.completed < state.submitted {
                return Err(EncoderError::Timeout("the encoder to drain"));
            }
        }
        Ok(())
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, State> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn cached_config(&self) -> EncoderResult<AvcConfig> {
        self.lock_state()
            .config
            .clone()
            .ok_or(EncoderError::MissingParameterSet("format description"))
    }

    /// Presentation timestamp of the next frame, strictly increasing.
    fn presentation_time(&mut self, timestamp: Duration) -> CMTime {
        let micros = i64::try_from(timestamp.as_micros()).unwrap_or(i64::MAX);
        let value = micros.max(self.last_pts.saturating_add(1));
        self.last_pts = value;
        // SAFETY: `CMTimeMake` has no preconditions for finite integer times.
        unsafe { CMTime::new(value, TIMESCALE) }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: invalidating a live session stops new callbacks before the
        // session and the shared state are released.
        unsafe { self.session.invalidate() };
    }
}

/// Reports the encoder this crate uses on macOS.
///
/// `VideoToolbox` selects the hardware or software H.264 encoder internally and
/// exposes no per-process alternative this crate could choose from.
///
/// # Errors
///
/// This backend always succeeds; the signature matches the platform contract.
#[allow(clippy::unnecessary_wraps)] // The platform-neutral API reports failures.
pub(crate) fn available_encoders() -> Result<Vec<String>, RecordingError> {
    Ok(vec!["VideoToolbox H.264".to_owned()])
}

/// Applies one `VideoToolbox` session property.
///
/// # Errors
///
/// Returns [`EncoderError::Status`] when the session rejects the property.
fn set_property(
    session: &VTCompressionSession,
    key: &CFString,
    value: &CFType,
) -> EncoderResult<()> {
    // SAFETY: the session is live and the property objects outlive the call.
    let status = unsafe { VTSessionSetProperty(session, key, Some(value)) };
    if status == 0 {
        Ok(())
    } else {
        Err(EncoderError::Status {
            status,
            operation: "configuring the encoder",
        })
    }
}

/// Source-frame attributes for the encoder's pixel buffer pool.
fn source_attributes(
    width: u32,
    height: u32,
) -> EncoderResult<CFRetained<CFDictionary<CFString, CFType>>> {
    let format = CFNumber::new_i32(
        i32::try_from(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange).unwrap_or(0),
    );
    let frame_width =
        CFNumber::new_i32(i32::try_from(width).map_err(|_| EncoderError::UnsupportedFrame)?);
    let frame_height =
        CFNumber::new_i32(i32::try_from(height).map_err(|_| EncoderError::UnsupportedFrame)?);
    // SAFETY: the keys are framework constants; the values are live.
    let keys: [&CFString; 4] = unsafe {
        [
            kCVPixelBufferPixelFormatTypeKey,
            kCVPixelBufferWidthKey,
            kCVPixelBufferHeightKey,
            kCVPixelBufferIOSurfacePropertiesKey,
        ]
    };
    let surface = CFDictionary::<CFString, CFType>::empty();
    let values: [&CFType; 4] = [&format, &frame_width, &frame_height, &surface];
    Ok(CFDictionary::from_slices(&keys, &values))
}

/// Frame properties that request a keyframe for the next input.
fn force_keyframe_properties() -> CFRetained<CFDictionary<CFString, CFType>> {
    // SAFETY: the key is a framework constant.
    let key = unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame };
    let value: &CFType = CFBoolean::new(true);
    CFDictionary::from_slices(&[key], &[value])
}

/// Receives compressed frames from `VideoToolbox`.
///
/// # Safety
///
/// `refcon` must be the live `Shared` passed to `VTCompressionSessionCreate`,
/// and it must outlive the session.
unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    _source_frame: *mut c_void,
    status: i32,
    _info_flags: VTEncodeInfoFlags,
    sample_buffer: *mut CMSampleBuffer,
) {
    // SAFETY: the creational contract above guarantees a live `Shared`.
    let shared = unsafe { &*refcon.cast::<Shared>() };
    let mut state = shared.state.lock().unwrap_or_else(PoisonError::into_inner);
    if status == 0
        && let Some(sample_buffer) = unsafe { sample_buffer.as_ref() }
    {
        match sample_from_buffer(sample_buffer) {
            Ok((sample, config)) => {
                if state.config.is_none() {
                    state.config = config;
                }
                if let Some(sample) = sample {
                    state.samples.push(sample);
                }
            }
            Err(error) => {
                state.failure = Some(io::Error::other(error));
            }
        }
    }
    state.completed = state.completed.saturating_add(1);
    shared.ready.notify_all();
}

/// Converts one output sample buffer into a container sample and configuration.
fn sample_from_buffer(
    sample_buffer: &CMSampleBuffer,
) -> EncoderResult<(Option<Sample>, Option<AvcConfig>)> {
    // SAFETY: the sample buffer is valid for the duration of the callback.
    let config = unsafe { sample_buffer.format_description() }
        .map(|description| config_from(&description))
        .transpose()?;
    let Some(data_buffer) = (unsafe { sample_buffer.data_buffer() }) else {
        return Ok((None, config));
    };
    let length = unsafe { data_buffer.data_length() };
    if length == 0 {
        return Ok((None, config));
    }
    let mut pointer: *mut std::ffi::c_char = ptr::null_mut();
    let mut contiguous = 0_usize;
    // SAFETY: the out pointers are valid; a block buffer that is not contiguous
    // for the whole sample is rejected below instead of being read as one span.
    let status = unsafe {
        data_buffer.data_pointer(0, &raw mut contiguous, ptr::null_mut(), &raw mut pointer)
    };
    if status != 0 || pointer.is_null() || contiguous < length {
        return Err(EncoderError::Status {
            status,
            operation: "reading the encoded frame",
        });
    }
    // SAFETY: the block buffer guarantees `length` readable bytes at `pointer`
    // for the lifetime of the sample buffer.
    let data = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), length) };
    let timestamp = unsafe { sample_buffer.presentation_time_stamp() };
    let sample = Sample::from_access_unit(data, milliseconds(timestamp), false)?;
    Ok((sample, config))
}

/// Reads the SPS and PPS out of an H.264 format description.
fn config_from(description: &CMFormatDescription) -> EncoderResult<AvcConfig> {
    let sequence = parameter_set(description, 0)
        .map_err(|_| EncoderError::MissingParameterSet("sequence parameter set"))?;
    let picture = parameter_set(description, 1)
        .map_err(|_| EncoderError::MissingParameterSet("picture parameter set"))?;
    Ok(AvcConfig::new(sequence, picture)?.with_centered_chroma()?)
}

/// Copies one parameter set out of an H.264 format description.
fn parameter_set(description: &CMFormatDescription, index: usize) -> EncoderResult<Vec<u8>> {
    let mut pointer: *const u8 = ptr::null();
    let mut size = 0_usize;
    let mut count = 0_usize;
    let mut length_size = 0_i32;
    // SAFETY: the format description is live and every out pointer is valid.
    let status = unsafe {
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            description,
            index,
            &raw mut pointer,
            &raw mut size,
            &raw mut count,
            &raw mut length_size,
        )
    };
    if status != 0 || count <= index || pointer.is_null() {
        return Err(EncoderError::MissingParameterSet("parameter set"));
    }
    if length_size != NAL_LENGTH_SIZE {
        return Err(EncoderError::UnsupportedLengthSize(length_size));
    }
    // SAFETY: the format description owns `size` readable parameter set bytes.
    Ok(unsafe { std::slice::from_raw_parts(pointer, size) }.to_vec())
}

/// Converts an encoder timestamp into milliseconds.
fn milliseconds(time: CMTime) -> u64 {
    let timescale = i64::from(time.timescale);
    if timescale <= 0 {
        return 0;
    }
    let value = time.value.max(0);
    u64::try_from(value.saturating_mul(1000) / timescale).unwrap_or(0)
}

/// Preserves the adapter error without exposing its type in public signatures.
fn encoder_failure(error: EncoderError) -> RecordingError {
    RecordingError::Encoder(Some(io::Error::other(error)))
}

#[cfg(test)]
mod tests;
