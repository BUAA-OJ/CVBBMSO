//! `ScreenCaptureKit` capture of the primary display.

use std::{
    io,
    ptr::NonNull,
    sync::{Arc, mpsc},
    time::Duration,
};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr};
use objc2::{
    AnyThread, DefinedClass, define_class, msg_send,
    rc::Retained,
    runtime::{NSObject, NSObjectProtocol, ProtocolObject},
};
use objc2_core_foundation::CFGetTypeID;
use objc2_core_graphics::{CGDisplayPixelsHigh, CGDisplayPixelsWide, CGMainDisplayID};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetDataSize, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetTypeID, CVPixelBufferGetWidth, CVPixelBufferIsPlanar,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSArray, NSError, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamOutput, SCStreamOutputType,
};

use super::{Frame, FrameSlot};
use crate::RecordingError;

/// `kCVPixelBufferLock_ReadOnly`: the captured image is only read, never written.
const READ_ONLY: CVPixelBufferLockFlags = CVPixelBufferLockFlags(1);
const BGRA_BYTES_PER_PIXEL: usize = 4;
/// How long the blocking setup waits for `ScreenCaptureKit`'s async steps.
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long dropping the backend waits for the stream to stop.
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// Failure inside the `ScreenCaptureKit` adapter.
#[derive(Debug, thiserror::Error)]
enum CaptureError {
    /// `ScreenCaptureKit` reported an error or the platform rejected the setup.
    #[error("screen capture is unavailable: {0}")]
    Unavailable(String),
    /// A display reports no pixels, so no frame could ever be captured.
    #[error("the primary display has no pixel dimensions")]
    EmptyDisplay,
}

type CaptureResult<T> = Result<T, CaptureError>;

/// Capture stream of the primary display, delivering the newest image.
pub(crate) struct Desktop {
    slot: Arc<FrameSlot>,
    // Declared before the handler so the stream that references it is released
    // first. The handler is also retained by the stream for its callbacks.
    stream: Retained<SCStream>,
    // Kept alive because `ScreenCaptureKit` does not retain stream outputs.
    handler: Retained<StreamHandler>,
    // The sample handler queue outlives every callback it dispatches.
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
    // The stream and its dispatch queue are used from the creating thread.
    _thread: std::marker::PhantomData<*const ()>,
}

impl Desktop {
    /// Opens the primary display for capture.
    ///
    /// The backend must stay on the thread that created it. `fps` bounds how
    /// often `ScreenCaptureKit` produces images; the sampler decides when to
    /// consume them.
    ///
    /// # Errors
    ///
    /// Returns [`RecordingError::Platform`] when screen-recording permission is
    /// missing, no display is available, or the stream cannot be started.
    pub(crate) fn open(fps: u32) -> Result<Self, RecordingError> {
        Self::open_inner(fps).map_err(failure)
    }

    /// Waits for the next captured image.
    ///
    /// Returns `Ok(None)` when no new image arrived within `timeout_ms`, which
    /// is a normal condition for a still screen rather than a fault. An image
    /// the sampler has not consumed yet is replaced by newer ones, so a slow
    /// consumer never records stale content.
    ///
    /// # Errors
    ///
    /// Returns [`RecordingError::Platform`] when the stream stopped, for example
    /// because the user revoked screen-recording permission.
    pub(crate) fn next_frame(&mut self, timeout_ms: u32) -> Result<Option<Frame>, RecordingError> {
        self.slot.next(Duration::from_millis(u64::from(timeout_ms)))
    }

    fn open_inner(fps: u32) -> CaptureResult<Self> {
        let fps = i32::try_from(fps.max(1)).unwrap_or(1);
        let display = primary_display()?;
        // SAFETY: a display object returned by ScreenCaptureKit is valid.
        let display_id = unsafe { display.displayID() };
        let width = u32::try_from(CGDisplayPixelsWide(display_id))
            .map_err(|_| CaptureError::EmptyDisplay)?;
        let height = u32::try_from(CGDisplayPixelsHigh(display_id))
            .map_err(|_| CaptureError::EmptyDisplay)?;
        if width == 0 || height == 0 {
            return Err(CaptureError::EmptyDisplay);
        }

        let slot = Arc::new(FrameSlot::new());
        let handler = StreamHandler::new(Arc::clone(&slot));

        // SAFETY: every ScreenCaptureKit object below is created here and kept
        // alive for the lifetime of the stream. The filter captures one display
        // with no excluded applications or windows.
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::new(),
                &NSArray::new(),
            )
        };
        // SAFETY: a fresh configuration object is owned here and fully configured
        // before the stream reads it.
        let configuration = unsafe { SCStreamConfiguration::new() };
        // SAFETY: the configuration object is live and the dimensions are the
        // display's pixel dimensions, which is what the captured buffer holds.
        unsafe {
            configuration.setWidth(usize::try_from(width).unwrap_or(0));
            configuration.setHeight(usize::try_from(height).unwrap_or(0));
            configuration.setPixelFormat(kCVPixelFormatType_32BGRA);
            configuration.setShowsCursor(false);
            configuration.setCapturesAudio(false);
            configuration.setMinimumFrameInterval(CMTime::new(1, fps));
        }

        let queue = DispatchQueue::new("cvbbmso.recorder.capture", DispatchQueueAttr::SERIAL);
        // SAFETY: the handler implements the delegate protocol for the lifetime
        // of the stream, which retains it through the protocol object.
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &configuration,
                Some(ProtocolObject::from_ref(&*handler)),
            )
        };

        let output: &ProtocolObject<dyn SCStreamOutput> = ProtocolObject::from_ref(&*handler);
        // SAFETY: the output implements the stream-output protocol and stays
        // alive with this backend; the queue is a live serial dispatch queue.
        unsafe {
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    output,
                    SCStreamOutputType::Screen,
                    Some(&queue),
                )
                .map_err(|error| CaptureError::Unavailable(description(&error)))?;
        }

        if let Err(error) = start_capture(&stream) {
            // SAFETY: the stream is live; stopping is best-effort on the error
            // path so a partially started capture cannot outlive the failure.
            unsafe { stream.stopCaptureWithCompletionHandler(None) };
            return Err(error);
        }
        Ok(Self {
            slot,
            stream,
            handler,
            _queue: queue,
            _thread: std::marker::PhantomData,
        })
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        let (send, receive) = mpsc::channel();
        let completion = RcBlock::new(move |_error: *mut NSError| {
            let _ = send.send(());
        });
        // SAFETY: the stream is live and the completion handler only signals the
        // channel. Waiting is bounded so a stuck stream cannot block shutdown.
        unsafe {
            self.stream
                .stopCaptureWithCompletionHandler(Some(&completion));
        }
        let _ = receive.recv_timeout(STOP_TIMEOUT);
    }
}

/// Waits until `ScreenCaptureKit` reports that the stream started.
fn start_capture(stream: &SCStream) -> CaptureResult<()> {
    let (send, receive) = mpsc::channel();
    let completion = RcBlock::new(move |error: *mut NSError| {
        // SAFETY: ScreenCaptureKit passes either NULL or a valid NSError for the
        // duration of this callback.
        let message = (!error.is_null()).then(|| description(unsafe { &*error }));
        let _ = send.send(message);
    });
    // SAFETY: the stream is live; the block is retained by ScreenCaptureKit
    // until it runs or the process exits.
    unsafe { stream.startCaptureWithCompletionHandler(Some(&completion)) };
    match receive.recv_timeout(SETUP_TIMEOUT) {
        Ok(None) => Ok(()),
        Ok(Some(message)) => Err(CaptureError::Unavailable(message)),
        Err(_) => Err(CaptureError::Unavailable(
            "the capture stream did not start".to_owned(),
        )),
    }
}

/// One retained display in transit from the query callback to the capture thread.
struct ShareableDisplay(Retained<SCDisplay>);

// SAFETY: SCDisplay holds read-only display metadata and has no main-thread
// affinity. The channel moves the retain without accessing the display, and
// Objective-C retain/release can run on either thread.
unsafe impl Send for ShareableDisplay {}

/// Resolves the display whose identifier matches the main display.
fn primary_display() -> CaptureResult<Retained<SCDisplay>> {
    let main_id = CGMainDisplayID();
    let (send, receive) = mpsc::channel();
    let completion = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // SAFETY: ScreenCaptureKit passes either NULL or valid objects for the
            // duration of this callback. The array gives us retained displays.
            let result = unsafe {
                if content.is_null() {
                    Err(if error.is_null() {
                        "no shareable content".to_owned()
                    } else {
                        description(&*error)
                    })
                } else {
                    let displays = (*content).displays();
                    displays
                        .to_vec()
                        .into_iter()
                        .find(|display| display.displayID() == main_id)
                        .map(ShareableDisplay)
                        .ok_or_else(|| "the primary display is not shareable".to_owned())
                }
            };
            let _ = send.send(result);
        },
    );
    // SAFETY: the completion handler is retained by ScreenCaptureKit until it
    // runs; the query only lists shareable displays.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            false,
            true,
            &completion,
        );
    }
    wait_for_display(receive, SETUP_TIMEOUT).map(|display| display.0)
}

fn wait_for_display<T>(
    receive: mpsc::Receiver<Result<T, String>>,
    timeout: Duration,
) -> CaptureResult<T> {
    let result = receive.recv_timeout(timeout);
    // Close the channel on timeout so any queued or later result is dropped.
    drop(receive);
    result
        .map_err(|_| CaptureError::Unavailable("the shareable content query timed out".to_owned()))?
        .map_err(CaptureError::Unavailable)
}

/// Message used when `ScreenCaptureKit` or Foundation reports an error.
fn description(error: &NSError) -> String {
    let message: Retained<NSString> = error.localizedDescription();
    message.to_string()
}

/// Copies one captured BGRA image out of its pixel buffer.
fn image_frame(sample_buffer: &CMSampleBuffer) -> Option<Frame> {
    // SAFETY: the sample buffer is live for the duration of the callback.
    let image = unsafe { sample_buffer.image_buffer() }?;
    pixel_buffer_frame(&image)
}

fn pixel_buffer_frame(image: &CVPixelBuffer) -> Option<Frame> {
    if CFGetTypeID(Some(image)) != CVPixelBufferGetTypeID()
        || CVPixelBufferGetPixelFormatType(image) != kCVPixelFormatType_32BGRA
        || CVPixelBufferIsPlanar(image)
    {
        return None;
    }

    let locked = ReadOnlyPixelBuffer::lock(image)?;
    locked.copy_bgra()
}

struct ReadOnlyPixelBuffer<'a>(&'a CVPixelBuffer);

impl<'a> ReadOnlyPixelBuffer<'a> {
    fn lock(image: &'a CVPixelBuffer) -> Option<Self> {
        // SAFETY: the caller checked this is a live pixel buffer. The guard
        // balances a successful lock with the same flags.
        if unsafe { CVPixelBufferLockBaseAddress(image, READ_ONLY) } != 0 {
            return None;
        }
        Some(Self(image))
    }

    fn copy_bgra(&self) -> Option<Frame> {
        let image = self.0;
        let width = CVPixelBufferGetWidth(image);
        let height = CVPixelBufferGetHeight(image);
        let frame_width = u32::try_from(width).ok()?;
        let frame_height = u32::try_from(height).ok()?;
        let pitch = CVPixelBufferGetBytesPerRow(image);
        let readable = CVPixelBufferGetDataSize(image);
        let base = CVPixelBufferGetBaseAddress(image).cast::<u8>();
        let (base, row_bytes) = packed_bgra_rows(base, width, height, pitch, readable)?;
        let mut pixels = Vec::new();
        pixels
            .try_reserve_exact(row_bytes.checked_mul(height)?)
            .ok()?;
        for row in 0..height {
            // SAFETY: the layout check bounded every visible row in the locked
            // packed buffer. Slice only the pixel bytes; padding need not be
            // initialized. The guard outlives each copy.
            let bytes =
                unsafe { std::slice::from_raw_parts(base.as_ptr().add(row * pitch), row_bytes) };
            pixels.extend_from_slice(bytes);
        }
        Some(Frame {
            width: frame_width,
            height: frame_height,
            pixels,
        })
    }
}

impl Drop for ReadOnlyPixelBuffer<'_> {
    fn drop(&mut self) {
        // SAFETY: construction succeeded with the same buffer and flags.
        unsafe { CVPixelBufferUnlockBaseAddress(self.0, READ_ONLY) };
    }
}

fn packed_bgra_rows(
    base: *mut u8,
    width: usize,
    height: usize,
    pitch: usize,
    readable: usize,
) -> Option<(NonNull<u8>, usize)> {
    let base = NonNull::new(base)?;
    let row_bytes = width.checked_mul(BGRA_BYTES_PER_PIXEL)?;
    if row_bytes == 0 || pitch < row_bytes {
        return None;
    }
    let used = height
        .checked_sub(1)?
        .checked_mul(pitch)?
        .checked_add(row_bytes)?;
    if used > readable || used > isize::MAX as usize {
        return None;
    }
    Some((base, row_bytes))
}

/// Preserves the adapter error without exposing its type in public signatures.
fn failure(error: CaptureError) -> RecordingError {
    RecordingError::Platform(Some(io::Error::other(error)))
}

/// State shared with the `ScreenCaptureKit` callbacks.
struct HandlerIvars {
    slot: Arc<FrameSlot>,
}

define_class!(
    /// Receives captured images and stream failures from `ScreenCaptureKit`.
    ///
    /// The class has no ivars of its own beyond the shared handles; every method
    /// is invoked on the sample handler queue configured by [`Desktop::open`].
    #[unsafe(super(NSObject))]
    #[name = "CVBBMSOStreamHandler"]
    #[ivars = HandlerIvars]
    struct StreamHandler;

    unsafe impl NSObjectProtocol for StreamHandler {}

    unsafe impl SCStreamOutput for StreamHandler {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn did_output_sample_buffer(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            output_type: SCStreamOutputType,
        ) {
            if output_type != SCStreamOutputType::Screen {
                return;
            }
            if let Some(frame) = image_frame(sample_buffer) {
                // Replaces an image the sampler has not consumed yet, so a slow
                // consumer always records the newest screen content.
                self.ivars().slot.store(frame);
            }
        }
    }

    unsafe impl SCStreamDelegate for StreamHandler {
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn did_stop_with_error(&self, _stream: &SCStream, error: &NSError) {
            self.ivars().slot.stop(description(error));
        }
    }
);

impl StreamHandler {
    fn new(slot: Arc<FrameSlot>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(HandlerIvars { slot });
        // SAFETY: NSObject's `init` accepts a fully initialized instance.
        unsafe { msg_send![super(this), init] }
    }
}

#[cfg(test)]
mod tests;
