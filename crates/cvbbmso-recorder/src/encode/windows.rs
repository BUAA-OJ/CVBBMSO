//! Media Foundation H.264 encoder discovery.

use std::ptr;

use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFMediaType, MF_MT_VIDEO_CHROMA_SITING, MF_MT_VIDEO_NOMINAL_RANGE,
    MF_MT_YUV_MATRIX, MF_VERSION, MFMediaType_Video, MFNominalRange_16_235, MFSTARTUP_LITE,
    MFShutdown, MFStartup, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG, MFT_ENUM_FLAG_ALL,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_FRIENDLY_NAME_Attribute, MFT_REGISTER_TYPE_INFO, MFTEnumEx,
    MFVideoChromaSubsampling_MPEG1, MFVideoChromaSubsampling_ProgressiveChroma, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFVideoTransferMatrix_BT709,
};
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::core::Result as WindowsResult;

use crate::RecordingError;

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
fn set_color_attributes(media_type: &IMFMediaType) -> WindowsResult<()> {
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
    query().map_err(|_| RecordingError::Encoder)
}

fn query() -> WindowsResult<Vec<String>> {
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

        let mut names = Vec::new();
        if !activations.is_null() {
            let entries =
                std::slice::from_raw_parts(activations, usize::try_from(count).unwrap_or(0));
            for activation in entries.iter().flatten() {
                names.push(name_of(activation)?);
            }
            CoTaskMemFree(Some(activations.cast()));
        }
        Ok(names)
    }
}

/// Reads the friendly name of one encoder activation.
fn name_of(activation: &IMFActivate) -> WindowsResult<String> {
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

use std::marker::PhantomData;
use std::mem::ManuallyDrop;

use super::Nv12Frame;
use windows::core::Error as WindowsError;

use windows::Win32::Media::MediaFoundation::{
    CMSH264EncoderMFT, IMFTransform, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_MT_AVG_BITRATE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MAX_KEYFRAME_SPACING, MF_MT_SUBTYPE, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFT_OUTPUT_DATA_BUFFER, MFVideoInterlace_Progressive,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};

/// Default bitrate for a desktop recording, in bits per second.
pub(crate) const DEFAULT_BITRATE: u32 = 4_000_000;

/// Configured H.264 encoder fed with NV12 frames.
pub(crate) struct Encoder {
    transform: IMFTransform,
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
        open_encoder(width, height, fps, bitrate).map_err(|_| RecordingError::Encoder)
    }
}

fn open_encoder(width: u32, height: u32, fps: u32, bitrate: u32) -> WindowsResult<Encoder> {
    // SAFETY: the guards live in the returned encoder, so Media Foundation and
    // the apartment outlive the transform they created.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let com = ComGuard;
        MFStartup(MF_VERSION, MFSTARTUP_LITE)?;
        let media = MediaFoundationGuard;

        let transform: IMFTransform =
            CoCreateInstance(&CMSH264EncoderMFT, None, CLSCTX_INPROC_SERVER)?;
        configure(&transform, width, height, fps, bitrate)?;
        Ok(Encoder {
            transform,
            _com: com,
            _media: media,
            _thread: PhantomData,
        })
    }
}

/// Negotiates the H.264 output type and the NV12 input type.
fn configure(
    transform: &IMFTransform,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
) -> WindowsResult<()> {
    // SAFETY: both media types are created here, fully described before they are
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

/// Packs two 32-bit halves into the 64-bit attribute layout Media Foundation uses.
fn pack(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

impl Encoder {
    /// Encodes one NV12 frame and returns the elementary stream bytes produced.
    ///
    /// Returns an empty vector when the encoder needs more input before it can
    /// emit a frame, which is normal for the first frames of a stream.
    ///
    /// # Errors
    ///
    /// Returns [`RecordingError::Encoder`] when the transform rejects the frame
    /// or cannot deliver output.
    pub(crate) fn encode(
        &mut self,
        frame: &Nv12Frame,
        timestamp: std::time::Duration,
    ) -> Result<Vec<u8>, RecordingError> {
        encode_frame(&self.transform, frame, timestamp).map_err(|_| RecordingError::Encoder)
    }
}

fn encode_frame(
    transform: &IMFTransform,
    frame: &Nv12Frame,
    timestamp: std::time::Duration,
) -> WindowsResult<Vec<u8>> {
    // SAFETY: the input buffer is locked only while it is written, and the output
    // sample is copied out before the contiguous buffer is unlocked.
    unsafe {
        let bytes = frame.as_bytes();
        let length = u32::try_from(bytes.len()).map_err(|_| WindowsError::from_win32())?;
        let buffer = MFCreateMemoryBuffer(length)?;
        let mut source_pixels: *mut u8 = ptr::null_mut();
        buffer.Lock(&raw mut source_pixels, None, None)?;
        std::slice::from_raw_parts_mut(source_pixels, bytes.len()).copy_from_slice(bytes);
        buffer.Unlock()?;
        buffer.SetCurrentLength(length)?;

        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(hns(timestamp))?;
        transform.ProcessInput(0, &sample, 0)?;

        let mut output = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(None),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        };
        let mut status = 0_u32;
        match transform.ProcessOutput(0, std::slice::from_mut(&mut output), &raw mut status) {
            Ok(()) => {}
            Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(Vec::new()),
            Err(error) => return Err(error),
        }
        let Some(output_sample) = ManuallyDrop::take(&mut output.pSample) else {
            return Ok(Vec::new());
        };

        let contiguous = output_sample.ConvertToContiguousBuffer()?;
        let mut encoded_bytes: *mut u8 = ptr::null_mut();
        let mut current = 0_u32;
        contiguous.Lock(&raw mut encoded_bytes, None, Some(&raw mut current))?;
        let bytes =
            std::slice::from_raw_parts(encoded_bytes, usize::try_from(current).unwrap_or(0))
                .to_vec();
        contiguous.Unlock()?;
        Ok(bytes)
    }
}

/// Converts a duration into the 100-nanosecond units Media Foundation uses.
fn hns(duration: std::time::Duration) -> i64 {
    i64::try_from(duration.as_nanos() / 100).unwrap_or(0)
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
    fn encodes_frames_into_an_elementary_stream() {
        let mut encoder =
            super::Encoder::open(320, 240, 15, super::DEFAULT_BITRATE).expect("encoder");
        let mut frame = crate::encode::Nv12Frame::new(320, 240).expect("frame");
        frame
            .convert_bgra(&vec![128_u8; 320 * 240 * 4])
            .expect("convert");
        let mut total = 0_usize;
        for index in 0..10_u32 {
            let bytes = encoder
                .encode(
                    &frame,
                    std::time::Duration::from_millis(u64::from(index) * 66),
                )
                .expect("encode");
            total += bytes.len();
        }
        println!("elementary stream bytes: {total}");
    }
}
