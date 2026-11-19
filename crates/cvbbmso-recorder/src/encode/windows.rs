//! Media Foundation H.264 encoder discovery.

use std::ptr;

use windows::Win32::Media::MediaFoundation::{
    IMFActivate, MF_VERSION, MFMediaType_Video, MFSTARTUP_LITE, MFShutdown, MFStartup,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG, MFT_ENUM_FLAG_ALL, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_FRIENDLY_NAME_Attribute, MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_H264,
    MFVideoFormat_NV12,
};
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::core::Result as WindowsResult;

use crate::RecordingError;

/// Longest friendly name this crate reads back from an encoder activation.
const NAME_LENGTH: usize = 128;

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

#[cfg(test)]
mod tests {
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
}
