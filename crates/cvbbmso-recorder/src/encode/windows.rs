//! Media Foundation H.264 encoder discovery and encoding.

use std::{marker::PhantomData, mem::ManuallyDrop, ptr};

use windows::Win32::Media::MediaFoundation::{
    CMSH264EncoderMFT, CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, ICodecAPI, IMFActivate, IMFMediaType, IMFTransform,
    MF_E_ATTRIBUTENOTFOUND, MF_E_BUFFERTOOSMALL, MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MAX_KEYFRAME_SPACING, MF_MT_MPEG_SEQUENCE_HEADER,
    MF_MT_SUBTYPE, MF_MT_VIDEO_CHROMA_SITING, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_YUV_MATRIX,
    MF_VERSION, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video,
    MFNominalRange_16_235, MFSTARTUP_LITE, MFSampleExtension_CleanPoint, MFShutdown, MFStartup,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG, MFT_ENUM_FLAG_ALL, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_COMMAND_DRAIN, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_DATA_BUFFER_NO_SAMPLE, MFT_OUTPUT_DATA_BUFFER_STREAM_END, MFT_OUTPUT_STREAM_INFO,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MFTEnumEx,
    MFVideoChromaSubsampling_MPEG1, MFVideoChromaSubsampling_ProgressiveChroma, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFVideoTransferMatrix_BT709,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows::Win32::System::Variant::{VARIANT, VT_UI4};
use windows::core::Error as WindowsError;
use windows::core::Interface;

use super::Nv12Frame;
use crate::RecordingError;
use crate::container::{
    AvcConfig, ContainerError, Sample, annex_b_nals, length_prefixed, nal_type,
};

/// Failure inside the Windows encoder adapter.
///
/// Platform calls keep their `windows::core::Error`; parameter-set and framing
/// failures keep the private container error instead of inventing a Win32 code.
#[derive(Debug, thiserror::Error)]
enum EncoderError {
    /// A COM or Media Foundation call failed.
    #[error(transparent)]
    Windows(#[from] WindowsError),
    /// The encoder emitted data that cannot form a valid AVC configuration.
    #[error("invalid encoder output: {0}")]
    Container(
        #[from]
        #[source]
        ContainerError,
    ),
    /// The encoder output omitted a required parameter set.
    #[error("encoder output is missing the {0}")]
    MissingParameterSet(&'static str),
    /// A frame or timestamp does not fit a Media Foundation field.
    #[error("frame does not fit the Media Foundation sample")]
    UnsupportedFrame,
}

type EncoderResult<T> = Result<T, EncoderError>;

/// Longest friendly name this crate reads back from an encoder activation.
const NAME_LENGTH: usize = 128;

/// Declares the converter's matrix, range and chroma location on a media type.
///
/// Apply to the NV12 input and H.264 output before type negotiation when the
/// encoder pipeline is connected. Setting these attributes is not proof that an
/// MFT writes matching VUI; the encoded stream must be checked during acceptance.
///
/// # Errors
///
/// Returns the original Windows error if an attribute cannot be stored.
fn set_color_attributes(media_type: &IMFMediaType) -> EncoderResult<()> {
    // SAFETY: the media type owns its writable attributes; each GUID and enum
    // value comes from Media Foundation. Chroma is centered by the 2x2 box filter.
    unsafe {
        media_type.SetUINT32(
            &MF_MT_YUV_MATRIX,
            MFVideoTransferMatrix_BT709.0.cast_unsigned(),
        )?;
        media_type.SetUINT32(
            &MF_MT_VIDEO_NOMINAL_RANGE,
            MFNominalRange_16_235.0.cast_unsigned(),
        )?;
        let siting =
            MFVideoChromaSubsampling_MPEG1.0 | MFVideoChromaSubsampling_ProgressiveChroma.0;
        media_type.SetUINT32(&MF_MT_VIDEO_CHROMA_SITING, siting.cast_unsigned())?;
    }
    Ok(())
}

/// Lists the H.264 encoders Media Foundation offers for NV12 input.
///
/// # Errors
///
/// Returns [`RecordingError::Encoder`] when Media Foundation cannot be started
/// or the encoder list cannot be read.
pub(crate) fn available_encoders() -> Result<Vec<String>, RecordingError> {
    query().map_err(encoder_failure)
}

fn query() -> EncoderResult<Vec<String>> {
    // SAFETY: the guards below pair the COM and Media Foundation initialisation
    // with their shutdown on every exit path, and the activation array returned
    // by `MFTEnumEx` is freed before returning.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let _com = ComGuard;
        MFStartup(MF_VERSION, MFSTARTUP_LITE)?;
        let _media = MediaFoundationGuard;

        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_ALL.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);

        let mut activations: *mut Option<IMFActivate> = ptr::null_mut();
        let mut count = 0_u32;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(&raw const input),
            Some(&raw const output),
            &raw mut activations,
            &raw mut count,
        )?;
        let activations = Activations::new(activations, count);

        let mut names = Vec::new();
        for activation in activations.as_slice().iter().flatten() {
            names.push(name_of(activation)?);
        }
        Ok(names)
    }
}

/// Owns the activation array returned by `MFTEnumEx`.
///
/// Both the array allocation and the interface reference in every element
/// belong to the caller, including when reading one name fails.
struct Activations {
    pointer: *mut Option<IMFActivate>,
    count: usize,
}

impl Activations {
    fn new(pointer: *mut Option<IMFActivate>, count: u32) -> Self {
        Self {
            pointer,
            count: usize::try_from(count).unwrap_or(0),
        }
    }

    fn as_slice(&self) -> &[Option<IMFActivate>] {
        if self.pointer.is_null() || self.count == 0 {
            return &[];
        }
        // SAFETY: `MFTEnumEx` returned a non-null array of `count` elements that
        // remains allocated until this guard drops.
        unsafe { std::slice::from_raw_parts(self.pointer, self.count) }
    }
}

impl Drop for Activations {
    fn drop(&mut self) {
        if self.pointer.is_null() {
            return;
        }
        // SAFETY: every element was initialized by `MFTEnumEx`; taking it drops
        // the interface reference, then the array itself is released once.
        unsafe {
            let entries = std::slice::from_raw_parts_mut(self.pointer, self.count);
            for entry in entries {
                entry.take();
            }
            CoTaskMemFree(Some(self.pointer.cast()));
        }
    }
}

/// Reads the friendly name of one encoder activation.
fn name_of(activation: &IMFActivate) -> EncoderResult<String> {
    let key = MFT_FRIENDLY_NAME_Attribute;
    let mut buffer = [0_u16; NAME_LENGTH];
    // SAFETY: the buffer is writable for its whole length and the key is a valid
    // Media Foundation attribute GUID.
    unsafe { activation.GetString(&raw const key, &mut buffer, None)? };
    let end = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(NAME_LENGTH);
    Ok(String::from_utf16_lossy(&buffer[..end]))
}

/// Keeps the calling thread's COM apartment initialised.
struct ComGuard;

impl Drop for ComGuard {
    fn drop(&mut self) {
        // SAFETY: every successful `CoInitializeEx` is balanced once here.
        unsafe { CoUninitialize() };
    }
}

/// Keeps Media Foundation started for the duration of the query.
struct MediaFoundationGuard;

impl Drop for MediaFoundationGuard {
    fn drop(&mut self) {
        // SAFETY: paire with the successful `MFStartup` above.
        let _ = unsafe { MFShutdown() };
    }
}

/// Default bitrate for a desktop recording, in bits per second.
pub(crate) const DEFAULT_BITRATE: u32 = 4_000_000;

/// Output stream layout reported by the transform.
struct OutputLayout {
    /// Whether the transform allocates output samples itself.
    provides_samples: bool,
    /// Minimum size of a caller-provided output sample, in bytes.
    buffer_size: u32,
}

/// Configured H.264 encoder fed with NV12 frames.
pub(crate) struct Encoder {
    transform: IMFTransform,
    /// Codec properties, including the per-frame keyframe request.
    codec: ICodecAPI,
    /// Frames between forced keyframes.
    gop: u32,
    /// Input frames accepted since the last keyframe request.
    frames_since_keyframe: u32,
    /// Sample duration in 100-nanosecond units, as Media Foundation requires.
    duration: i64,
    layout: OutputLayout,
    // Declared after the transform so the transform is released first.
    _com: ComGuard,
    _media: MediaFoundationGuard,
    // The transform belongs to the thread whose apartment created it.
    _thread: PhantomData<*const ()>,
}

impl Encoder {
    /// Opens an H.264 encoder for `width` by `height` frames at `fps`.
    ///
    /// # Errors
    ///
    /// Returns [`RecordingError::Encoder`] when Media Foundation cannot start,
    /// no encoder can be created, or the media types are rejected.
    pub(crate) fn open(
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
    ) -> Result<Self, RecordingError> {
        open_encoder(width, height, fps, bitrate).map_err(encoder_failure)
    }

    /// Encodes one NV12 frame and returns the samples produced for it.
    ///
    /// An empty result is normal while the encoder buffers its first frames.
    /// Timestamps come from Media Foundation's output samples and are relative
    /// to the first input frame.
    ///
    /// # Errors
    ///
    /// Returns [`RecordingError::Encoder`] when the transform rejects the frame,
    /// cannot deliver output, or emits a malformed elementary stream.
    pub(crate) fn encode(
        &mut self,
        frame: &Nv12Frame,
        timestamp: std::time::Duration,
    ) -> Result<Vec<Sample>, RecordingError> {
        self.encode_inner(frame, timestamp).map_err(encoder_failure)
    }

    /// Drains the frames the encoder still holds after the last input.
    ///
    /// # Errors
    ///
    /// Returns [`RecordingError::Encoder`] when the transform cannot be drained.
    pub(crate) fn flush(&mut self) -> Result<Vec<Sample>, RecordingError> {
        self.flush_inner().map_err(encoder_failure)
    }

    /// Requests that the next encoded frame be a keyframe.
    ///
    /// The recorder uses this when rotating to a new segment so the segment can
    /// be decoded independently of the previous one.
    ///
    /// # Errors
    ///
    /// Returns [`RecordingError::Encoder`] when the codec rejects the request.
    pub(crate) fn force_keyframe(&mut self) -> Result<(), RecordingError> {
        self.request_keyframe().map_err(encoder_failure)
    }

    /// Builds the AVC configuration record from the negotiated output type.
    ///
    /// # Errors
    ///
    /// Returns [`RecordingError::Encoder`] when the transform exposes no
    /// sequence header or the parameter sets are malformed.
    pub(crate) fn config(&self) -> Result<AvcConfig, RecordingError> {
        config_of(&self.transform).map_err(encoder_failure)
    }

    fn encode_inner(
        &mut self,
        frame: &Nv12Frame,
        timestamp: std::time::Duration,
    ) -> EncoderResult<Vec<Sample>> {
        let mut samples = Vec::new();
        if self.frames_since_keyframe >= self.gop {
            self.request_keyframe()?;
        }
        let bytes = frame.as_bytes();
        let sample = input_sample(bytes, timestamp, self.duration)?;
        loop {
            match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
                Ok(()) => break,
                // The transform still holds an undisplayed frame. Drain it and
                // retry the same input sample, which was not consumed.
                Err(error) if error.code() == MF_E_NOTACCEPTING => {
                    samples.extend(drain(&self.transform, &mut self.layout)?);
                }
                Err(error) => return Err(error.into()),
            }
        }
        self.frames_since_keyframe = self.frames_since_keyframe.saturating_add(1);
        samples.extend(drain(&self.transform, &mut self.layout)?);
        Ok(samples)
    }

    fn flush_inner(&mut self) -> EncoderResult<Vec<Sample>> {
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
        }
        drain(&self.transform, &mut self.layout)
    }

    fn request_keyframe(&mut self) -> EncoderResult<()> {
        set_codec_u32(&self.codec, &CODECAPI_AVEncVideoForceKeyFrame, 1)?;
        self.frames_since_keyframe = 0;
        Ok(())
    }
}

fn open_encoder(width: u32, height: u32, fps: u32, bitrate: u32) -> EncoderResult<Encoder> {
    // SAFETY: the guards live in the returned encoder, so Media Foundation and
    // the apartment outlive the transform they created.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let com = ComGuard;
        MFStartup(MF_VERSION, MFSTARTUP_LITE)?;
        let media = MediaFoundationGuard;

        let transform: IMFTransform =
            CoCreateInstance(&CMSH264EncoderMFT, None, CLSCTX_INPROC_SERVER)?;
        let codec: ICodecAPI = transform.cast()?;
        configure_b_frames(&codec)?;
        configure_output(&transform, width, height, fps, bitrate)?;
        configure_input(&transform, width, height, fps)?;
        configure_gop(&codec, fps)?;
        let layout = output_layout(&transform)?;
        Ok(Encoder {
            transform,
            codec,
            gop: fps.saturating_mul(2).max(1),
            frames_since_keyframe: 0,
            duration: sample_duration(fps),
            layout,
            _com: com,
            _media: media,
            _thread: PhantomData,
        })
    }
}

/// Turns off B-frames so output timestamps are monotonic and no PTS/DTS
/// reordering is needed in the containers this crate writes.
///
/// This property must be set before the output type; afterwards the encoder
/// rejects or ignores the change.
fn configure_b_frames(codec: &ICodecAPI) -> EncoderResult<()> {
    set_codec_u32(codec, &CODECAPI_AVEncMPVDefaultBPictureCount, 0)
}

/// Requests a keyframe every two seconds through the codec property.
///
/// The Microsoft encoder does not reliably honor this hint, so `Encoder::encode`
/// also forces the keyframe explicitly at the same interval.
fn configure_gop(codec: &ICodecAPI, fps: u32) -> EncoderResult<()> {
    set_codec_u32(
        codec,
        &CODECAPI_AVEncMPVGOPSize,
        fps.saturating_mul(2).max(1),
    )
}

/// Sets one `ICodecAPI` property to a `VT_UI4` value.
fn set_codec_u32(codec: &ICodecAPI, key: &windows::core::GUID, value: u32) -> EncoderResult<()> {
    // SAFETY: the caller holds a live codec interface. The variant owns no
    // resources and is only read by `SetValue`.
    unsafe {
        codec.IsSupported(key)?;
        let mut variant = VARIANT::default();
        let inner = &mut *variant.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = value;
        codec.SetValue(key, &raw const variant)?;
    }
    Ok(())
}

/// Negotiates the H.264 output type.
fn configure_output(
    transform: &IMFTransform,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
) -> EncoderResult<()> {
    // SAFETY: the media type is created here, fully described before it is
    // handed to the transform, and dropped after negotiation.
    unsafe {
        let output = MFCreateMediaType()?;
        output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
        output.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
        output.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
        output.SetUINT32(
            &MF_MT_INTERLACE_MODE,
            MFVideoInterlace_Progressive.0.cast_unsigned(),
        )?;
        // A keyframe every two seconds keeps segment boundaries seekable.
        output.SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, fps.saturating_mul(2))?;
        set_color_attributes(&output)?;
        transform.SetOutputType(0, &output, 0)?;
    }
    Ok(())
}

/// Negotiates the NV12 input type after codec properties are fixed.
fn configure_input(
    transform: &IMFTransform,
    width: u32,
    height: u32,
    fps: u32,
) -> EncoderResult<()> {
    // SAFETY: the media type is created here, fully described before it is
    // handed to the transform, and dropped after negotiation.
    unsafe {
        let input = MFCreateMediaType()?;
        input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        input.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
        input.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
        input.SetUINT32(
            &MF_MT_INTERLACE_MODE,
            MFVideoInterlace_Progressive.0.cast_unsigned(),
        )?;
        set_color_attributes(&input)?;
        transform.SetInputType(0, &input, 0)?;
    }
    Ok(())
}

/// Reads the output stream layout once after type negotiation.
fn output_layout(transform: &IMFTransform) -> EncoderResult<OutputLayout> {
    // SAFETY: the stream identifier was validated by the successful negotiation.
    let info: MFT_OUTPUT_STREAM_INFO = unsafe { transform.GetOutputStreamInfo(0)? };
    Ok(OutputLayout {
        provides_samples: info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0.cast_unsigned() != 0,
        buffer_size: info.cbSize.max(1),
    })
}

/// Packs two 32-bit halves into the 64-bit attribute layout Media Foundation uses.
fn pack(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

/// Reads the H.264 sequence header Media Foundation attaches to the output type.
fn sequence_header(transform: &IMFTransform) -> EncoderResult<Option<Vec<u8>>> {
    // SAFETY: the media type and its blob are owned by the transform, and the
    // buffer is sized from the reported attribute length before it is read.
    unsafe {
        let media_type = transform.GetOutputCurrentType(0)?;
        read_sequence_header(&media_type)
    }
}

fn read_sequence_header(media_type: &IMFMediaType) -> EncoderResult<Option<Vec<u8>>> {
    // SAFETY: the caller owns a live media type; the buffer is sized from the
    // attribute length before it is read.
    unsafe {
        let key = MF_MT_MPEG_SEQUENCE_HEADER;
        let size = match media_type.GetBlobSize(&raw const key) {
            Ok(size) => size,
            Err(error) if error.code() == MF_E_ATTRIBUTENOTFOUND => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut header = vec![0_u8; usize::try_from(size).unwrap_or(0)];
        media_type.GetBlob(&raw const key, &mut header, None)?;
        Ok(Some(header))
    }
}

/// Builds the AVC configuration record from the negotiated output type.
fn config_of(transform: &IMFTransform) -> EncoderResult<AvcConfig> {
    build_config(sequence_header(transform)?)
}

/// Builds a configuration record from the parameter sets in a sequence header.
fn build_config(header: Option<Vec<u8>>) -> EncoderResult<AvcConfig> {
    let header = header.ok_or(EncoderError::MissingParameterSet("sequence header"))?;
    let units = annex_b_nals(&header);
    let sequence = units
        .iter()
        .find(|unit| nal_type(unit) == 7)
        .ok_or(EncoderError::MissingParameterSet("sequence parameter set"))?;
    let picture = units
        .iter()
        .find(|unit| nal_type(unit) == 8)
        .ok_or(EncoderError::MissingParameterSet("picture parameter set"))?;
    Ok(AvcConfig::new(sequence.to_vec(), picture.to_vec())?)
}

/// Converts a frame rate into the 100-nanosecond sample duration.
fn sample_duration(fps: u32) -> i64 {
    i64::try_from(10_000_000_u64 / u64::from(fps.max(1))).unwrap_or(1)
}

/// Creates the input sample holding one compact NV12 frame.
fn input_sample(
    bytes: &[u8],
    timestamp: std::time::Duration,
    duration: i64,
) -> EncoderResult<windows::Win32::Media::MediaFoundation::IMFSample> {
    let length = u32::try_from(bytes.len()).map_err(|_| EncoderError::UnsupportedFrame)?;
    // SAFETY: the buffer is locked only while it is written and unlocked before
    // it is attached to the sample.
    unsafe {
        let buffer = MFCreateMemoryBuffer(length)?;
        let mut destination: *mut u8 = ptr::null_mut();
        buffer.Lock(&raw mut destination, None, None)?;
        std::slice::from_raw_parts_mut(destination, bytes.len()).copy_from_slice(bytes);
        buffer.Unlock()?;
        buffer.SetCurrentLength(length)?;

        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(hns(timestamp)?)?;
        sample.SetSampleDuration(duration)?;
        Ok(sample)
    }
}

/// Pulls every sample the transform can produce without more input.
fn drain(transform: &IMFTransform, layout: &mut OutputLayout) -> EncoderResult<Vec<Sample>> {
    let mut samples = Vec::new();
    loop {
        let mut output = output_buffer(layout)?;
        let mut status = 0_u32;
        // SAFETY: `output` describes one stream and outlives the call. A sample
        // the transform provides is taken back before the struct is dropped.
        let result = unsafe {
            transform.ProcessOutput(0, std::slice::from_mut(&mut output), &raw mut status)
        };
        // SAFETY: each `ManuallyDrop` field is taken exactly once, so the struct
        // does not release a sample or event collection that is now owned here.
        let (sample, events) = unsafe {
            (
                ManuallyDrop::take(&mut output.pSample),
                ManuallyDrop::take(&mut output.pEvents),
            )
        };
        drop(events);
        match result {
            Ok(()) => {
                let no_sample =
                    output.dwStatus == MFT_OUTPUT_DATA_BUFFER_NO_SAMPLE.0.cast_unsigned();
                if !no_sample
                    && let Some(sample) = sample
                    && let Some(sample) = normalize(&sample)?
                {
                    samples.push(sample);
                }
                let stream_end =
                    output.dwStatus & MFT_OUTPUT_DATA_BUFFER_STREAM_END.0.cast_unsigned() != 0;
                if stream_end {
                    return Ok(samples);
                }
            }
            Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(samples),
            Err(error) if error.code() == MF_E_BUFFERTOOSMALL => {
                // The required output buffer grew past the stream info estimate;
                // retry with a larger sample before failing the recording.
                if layout.provides_samples {
                    return Err(error.into());
                }
                match layout.buffer_size.checked_mul(2) {
                    Some(size) => layout.buffer_size = size,
                    None => return Err(error.into()),
                }
            }
            Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // The encoder chose a different output type after seeing the
                // first frame; negotiate it and continue draining.
                // SAFETY: the transform reports a valid output stream type.
                unsafe {
                    let available = transform.GetOutputAvailableType(0, 0)?;
                    transform.SetOutputType(0, &available, 0)?;
                }
                *layout = output_layout(transform)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Allocates the output buffer the transform expects, or none when it provides
/// its own samples.
fn output_buffer(layout: &OutputLayout) -> EncoderResult<MFT_OUTPUT_DATA_BUFFER> {
    let sample = if layout.provides_samples {
        None
    } else {
        // SAFETY: the buffer is attached to a sample owned by the local struct.
        unsafe {
            let buffer = MFCreateMemoryBuffer(layout.buffer_size)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            Some(sample)
        }
    };
    Ok(MFT_OUTPUT_DATA_BUFFER {
        dwStreamID: 0,
        pSample: ManuallyDrop::new(sample),
        dwStatus: 0,
        pEvents: ManuallyDrop::new(None),
    })
}

/// Normalises one encoder output sample into a container-ready sample.
fn normalize(
    sample: &windows::Win32::Media::MediaFoundation::IMFSample,
) -> EncoderResult<Option<Sample>> {
    // SAFETY: the sample is live. A missing timestamp is a broken encoder
    // contract because the input carried both time and duration; refusing it
    // keeps the container timeline ordered.
    let timestamp = unsafe { sample.GetSampleTime() }?;
    let clean_point =
        unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.is_ok_and(|flag| flag != 0);
    // SAFETY: the sample owns a contiguous buffer for the duration of the call.
    let contiguous = unsafe { sample.ConvertToContiguousBuffer()? };
    let mut bytes: *mut u8 = ptr::null_mut();
    let mut length = 0_u32;
    // SAFETY: the contiguous buffer is locked while its bytes are copied out and
    // unlocked before this function returns, whatever the parse result.
    unsafe {
        contiguous.Lock(&raw mut bytes, None, Some(&raw mut length))?;
        let stream = std::slice::from_raw_parts(bytes, usize::try_from(length).unwrap_or(0));
        let normalized = frame_sample(stream, timestamp, clean_point);
        contiguous.Unlock()?;
        normalized
    }
}

/// Converts one Annex B elementary-stream segment into a framed sample.
fn frame_sample(stream: &[u8], timestamp: i64, clean_point: bool) -> EncoderResult<Option<Sample>> {
    let units = annex_b_nals(stream);
    if units.is_empty() {
        return Ok(None);
    }
    let unit_keyframe = units.iter().any(|unit| nal_type(unit) == 5);
    let data = length_prefixed(&units)?;
    Ok(Some(Sample {
        timestamp_ms: u64::try_from(timestamp.max(0) / 10_000).unwrap_or(0),
        keyframe: clean_point || unit_keyframe,
        data,
    }))
}

/// Converts a duration into the 100-nanosecond units Media Foundation uses.
fn hns(duration: std::time::Duration) -> EncoderResult<i64> {
    i64::try_from(duration.as_nanos() / 100).map_err(|_| EncoderError::UnsupportedFrame)
}

/// Preserves the adapter error without exposing its type in public signatures.
fn encoder_failure(error: EncoderError) -> RecordingError {
    RecordingError::Encoder(Some(std::io::Error::other(error)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Media::MediaFoundation::MFCreateMediaType;

    #[test]
    fn color_attributes_match_the_bt709_limited_centered_conversion() {
        // SAFETY: initialize the test thread and keep all media types within
        // the lifetime of their COM and Media Foundation guards.
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .expect("COM");
            let _com = ComGuard;
            MFStartup(MF_VERSION, MFSTARTUP_LITE).expect("Media Foundation");
            let _media = MediaFoundationGuard;
            let media_type = MFCreateMediaType().expect("media type");
            set_color_attributes(&media_type).expect("color description");
            assert_eq!(media_type.GetUINT32(&MF_MT_YUV_MATRIX).expect("matrix"), 1);
            assert_eq!(
                media_type
                    .GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE)
                    .expect("range"),
                2
            );
            // Progressive, U/V aligned, centered in both axes.
            assert_eq!(
                media_type
                    .GetUINT32(&MF_MT_VIDEO_CHROMA_SITING)
                    .expect("chroma siting"),
                9
            );
        }
    }

    #[test]
    fn frame_sample_frames_nals_and_detects_a_keyframe() {
        let stream = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5,
        ];

        let sample = frame_sample(&stream, 660_000, false)
            .expect("framing")
            .expect("present");

        assert_eq!(sample.timestamp_ms, 66);
        assert!(sample.keyframe);
        assert_eq!(
            sample.data,
            [
                0, 0, 0, 3, 0x67, 1, 2, 0, 0, 0, 2, 0x68, 3, 0, 0, 0, 3, 0x65, 4, 5
            ]
        );
    }

    #[test]
    fn frame_sample_treats_a_clean_point_as_a_keyframe_and_skips_empty_output() {
        let stream = [0, 0, 0, 1, 0x41, 1];

        let sample = frame_sample(&stream, 0, true)
            .expect("framing")
            .expect("present");

        assert!(sample.keyframe);
        assert!(
            frame_sample(&[], 0, false)
                .expect("empty is not an error")
                .is_none()
        );
    }

    #[test]
    fn missing_parameter_sets_name_the_missing_part() {
        assert!(matches!(
            build_config(None),
            Err(EncoderError::MissingParameterSet("sequence header"))
        ));
        let sequence_only = vec![0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x1E, 0xD9, 0x01];
        assert!(matches!(
            build_config(Some(sequence_only)),
            Err(EncoderError::MissingParameterSet("picture parameter set"))
        ));
    }

    #[test]
    fn malformed_parameter_sets_keep_the_container_cause() {
        let truncated = vec![0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xCE];
        let error = build_config(Some(truncated)).expect_err("truncated parameter sets");
        assert!(matches!(
            error,
            EncoderError::Container(ContainerError::InvalidParameterSet)
        ));

        let recording = encoder_failure(error);
        let RecordingError::Encoder(Some(io)) = recording else {
            panic!("expected an encoder category");
        };
        let encoder = io.get_ref().expect("encoder cause");
        assert!(encoder.downcast_ref::<EncoderError>().is_some());
        let container = encoder.source().expect("container cause");
        assert!(container.downcast_ref::<ContainerError>().is_some());
    }

    #[test]
    #[ignore = "requires Media Foundation"]
    fn keyframes_are_at_most_two_seconds_apart() {
        let mut encoder =
            super::Encoder::open(160, 120, 15, super::DEFAULT_BITRATE).expect("encoder");
        let mut frame = crate::encode::Nv12Frame::new(160, 120).expect("frame");
        frame
            .convert_bgra(&vec![128_u8; 160 * 120 * 4])
            .expect("convert");
        let mut produced = 0_usize;
        let mut keyframes = Vec::new();
        for index in 0..150_u32 {
            for sample in encoder
                .encode(
                    &frame,
                    std::time::Duration::from_millis(u64::from(index) * 66),
                )
                .expect("encode")
            {
                if sample.keyframe {
                    keyframes.push(produced);
                }
                produced += 1;
            }
        }
        for sample in encoder.flush().expect("flush") {
            if sample.keyframe {
                keyframes.push(produced);
            }
            produced += 1;
        }

        assert_eq!(produced, 150);
        assert_eq!(keyframes.first(), Some(&0));
        for pair in keyframes.windows(2) {
            assert!(
                pair[1] - pair[0] <= 30,
                "keyframes are more than two seconds apart: {keyframes:?}"
            );
        }
    }

    #[test]
    #[ignore = "requires Media Foundation"]
    fn lists_h264_encoders() {
        let names = super::available_encoders().expect("enumerate encoders");

        println!("encoders: {names:#?}");
        assert!(
            !names.is_empty(),
            "Media Foundation reported no H.264 encoder"
        );
    }

    #[test]
    #[ignore = "requires Media Foundation"]
    fn opens_and_configures_an_h264_encoder() {
        let encoder =
            super::Encoder::open(1920, 1080, 15, super::DEFAULT_BITRATE).expect("encoder");
        drop(encoder);
    }

    #[test]
    #[ignore = "requires Media Foundation"]
    fn merges_encoded_segments_into_a_movie() {
        use crate::container::MkvWriter;
        use crate::store::SegmentStore;
        use sha2::Digest as _;
        use std::fmt::Write as _;

        let directory = tempfile::TempDir::new().expect("temp dir");
        let mut store = SegmentStore::open(directory.path()).expect("store");
        let mut frame = crate::encode::Nv12Frame::new(320, 240).expect("frame");
        frame
            .convert_bgra(&vec![128_u8; 320 * 240 * 4])
            .expect("convert");
        for index in 1..=2_u32 {
            let mut encoder =
                super::Encoder::open(320, 240, 15, super::DEFAULT_BITRATE).expect("encoder");
            let config = encoder.config().expect("sequence header");
            let mut writer = MkvWriter::new(config, 320, 240);
            for frame_index in 0..5_u32 {
                let samples = encoder
                    .encode(
                        &frame,
                        std::time::Duration::from_millis(u64::from(frame_index) * 66),
                    )
                    .expect("encode");
                for sample in samples {
                    writer.push(sample).expect("push");
                }
            }
            for sample in encoder.flush().expect("flush") {
                writer.push(sample).expect("push");
            }
            std::fs::write(store.video_path(index), writer.finish().expect("render"))
                .expect("segment file");
            store
                .seal(
                    index,
                    time::OffsetDateTime::from_unix_timestamp(0).expect("start"),
                    time::OffsetDateTime::from_unix_timestamp(30).expect("end"),
                )
                .expect("seal");
        }

        let digest = store.merge().expect("merge");
        let final_path = store.final_path();
        let bytes = std::fs::read(&final_path).expect("final video");
        assert_ne!(bytes.len(), 0);
        let mut computed = String::new();
        for byte in sha2::Sha256::digest(&bytes) {
            let _ = write!(computed, "{byte:02x}");
        }
        assert_eq!(digest.as_str(), computed);
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../target/recorder-m6-check.mp4"
            ),
            &bytes,
        )
        .expect("acceptance artifact");
    }

    #[test]
    #[ignore = "requires Media Foundation"]
    fn encodes_frames_into_an_elementary_stream() {
        use crate::container::{MkvFile, MkvWriter};

        let mut encoder =
            super::Encoder::open(320, 240, 15, super::DEFAULT_BITRATE).expect("encoder");
        let config = encoder.config().expect("sequence header");
        let mut writer = MkvWriter::new(config, 320, 240);
        let mut frame = crate::encode::Nv12Frame::new(320, 240).expect("frame");
        frame
            .convert_bgra(&vec![128_u8; 320 * 240 * 4])
            .expect("convert");
        let mut produced = 0_usize;
        for index in 0..10_u32 {
            let samples = encoder
                .encode(
                    &frame,
                    std::time::Duration::from_millis(u64::from(index) * 66),
                )
                .expect("encode");
            for sample in samples {
                produced += 1;
                writer.push(sample).expect("push");
            }
        }
        for sample in encoder.flush().expect("flush") {
            produced += 1;
            writer.push(sample).expect("push");
        }
        let bytes = writer.finish().expect("render");
        let parsed = MkvFile::parse(&bytes).expect("parse");
        assert_eq!(produced, parsed.samples.len());
        assert_ne!(parsed.samples.len(), 0);
        assert!(parsed.samples[0].keyframe);
        println!(
            "samples: {produced}, first: {} bytes, file: {} bytes",
            parsed.samples[0].data.len(),
            bytes.len()
        );
    }
}
