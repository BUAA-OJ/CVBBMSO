//! `ScreenCaptureKit` capture of the primary display.

use std::{
    io,
    sync::{
        Arc, Mutex, PoisonError,
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    },
    time::Duration,
};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr};
use objc2::{
    AnyThread, DefinedClass, define_class, msg_send,
    rc::Retained,
    runtime::{NSObject, NSObjectProtocol, ProtocolObject},
};
use objc2_core_graphics::{CGDisplayPixelsHigh, CGDisplayPixelsWide, CGMainDisplayID};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSArray, NSError, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamOutput, SCStreamOutputType,
};

use super::Frame;
use crate::RecordingError;

/// `kCVPixelBufferLock_ReadOnly`: the captured image is only read, never written.
const READ_ONLY: CVPixelBufferLockFlags = CVPixelBufferLockFlags(1);
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
    frames: Receiver<Frame>,
    stopped: Arc<Mutex<Option<String>>>,
    // Declared before the handler so the stream that references it is released
    // first. The handler is also retained by the stream for its callbacks.
    stream: Retained<SCStream>,
    // Kept alive because ScreenCaptureKit does not retain stream outputs.
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
    /// is a normal condition for a still screen rather than a fault.
    ///
    /// # Errors
    ///
    /// Returns [`RecordingError::Platform`] when the stream stopped, for example
    /// because the user revoked screen-recording permission.
    pub(crate) fn next_frame(&mut self, timeout_ms: u32) -> Result<Option<Frame>, RecordingError> {
        if let Some(message) = self.take_stopped() {
            return Err(failure(CaptureError::Unavailable(message)));
        }
        match self
            .frames
            .recv_timeout(Duration::from_millis(u64::from(timeout_ms)))
        {
            Ok(frame) => Ok(Some(frame)),
            Err(RecvTimeoutError::Timeout) => match self.take_stopped() {
                Some(message) => Err(failure(CaptureError::Unavailable(message))),
                None => Ok(None),
            },
            Err(RecvTimeoutError::Disconnected) => Err(failure(CaptureError::Unavailable(
                "the capture stream stopped".to_owned(),
            ))),
        }
    }

    fn take_stopped(&self) -> Option<String> {
        self.stopped
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
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

        let (sender, frames) = mpsc::sync_channel(1);
        let stopped = Arc::new(Mutex::new(None));
        let handler = StreamHandler::new(sender, Arc::clone(&stopped));

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
            configuration.setQueueDepth(1);
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
            frames,
            stopped,
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

/// Resolves the display whose identifier matches the main display.
fn primary_display() -> CaptureResult<Retained<SCDisplay>> {
    let main_id = CGMainDisplayID();
    let (send, receive) = mpsc::channel();
    let completion = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // SAFETY: ScreenCaptureKit passes either NULL or valid objects for the
            // duration of this callback. The display is retained before it crosses
            // the channel as an owned pointer.
            let result = unsafe {
                if content.is_null() {
                    Err(if error.is_null() {
                        "no shareable content".to_owned()
                    } else {
                        description(&*error)
                    })
                } else {
                    let displays = (*content).displays();
                    match displays
                        .to_vec()
                        .into_iter()
                        .find(|display| display.displayID() == main_id)
                    {
                        Some(display) => Ok(Retained::into_raw(display) as usize),
                        None => Err("the primary display is not shareable".to_owned()),
                    }
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
    let captured = receive.recv_timeout(SETUP_TIMEOUT).map_err(|_| {
        CaptureError::Unavailable("the shareable content query timed out".to_owned())
    })?;
    let pointer = captured.map_err(CaptureError::Unavailable)? as *mut SCDisplay;
    // SAFETY: the block transferred exactly one retain of this display.
    unsafe { Retained::from_raw(pointer) }
        .ok_or_else(|| CaptureError::Unavailable("the primary display is gone".to_owned()))
}

/// Message used when `ScreenCaptureKit` or Foundation reports an error.
fn description(error: &NSError) -> String {
    let message: Retained<NSString> = error.localizedDescription();
    message.to_string()
}

/// Copies one captured BGRA image out of its pixel buffer.
fn image_frame(sample_buffer: &CMSampleBuffer) -> Option<Frame> {
    // SAFETY: the sample buffer belongs to ScreenCaptureKit for the duration of
    // the callback; the pixel buffer is locked while its bytes are read.
    let image = unsafe { sample_buffer.image_buffer() }?;
    let width = u32::try_from(CVPixelBufferGetWidth(&image)).ok()?;
    let height = u32::try_from(CVPixelBufferGetHeight(&image)).ok()?;
    if unsafe { CVPixelBufferLockBaseAddress(&image, READ_ONLY) } != 0 {
        return None;
    }
    let pitch = CVPixelBufferGetBytesPerRow(&image);
    let base = CVPixelBufferGetBaseAddress(&image).cast::<u8>();
    // SAFETY: a successful read-only lock guarantees `pitch * height` readable
    // bytes at the base address until the matching unlock below.
    let bytes = unsafe { std::slice::from_raw_parts(base, pitch.checked_mul(height as usize)?) };
    let frame = Frame::from_padded_bgra(bytes, width, height, pitch);
    unsafe { CVPixelBufferUnlockBaseAddress(&image, READ_ONLY) };
    frame
}

/// Preserves the adapter error without exposing its type in public signatures.
fn failure(error: CaptureError) -> RecordingError {
    RecordingError::Platform(Some(io::Error::other(error)))
}

/// State shared with the `ScreenCaptureKit` callbacks.
struct HandlerIvars {
    frames: SyncSender<Frame>,
    stopped: Arc<Mutex<Option<String>>>,
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
                // A full channel means the sampler has not consumed the previous
                // image yet. Dropping the newest keeps the pull model bounded.
                let _ = self.ivars().frames.try_send(frame);
            }
        }
    }

    unsafe impl SCStreamDelegate for StreamHandler {
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn did_stop_with_error(&self, _stream: &SCStream, error: &NSError) {
            let mut slot = self
                .ivars()
                .stopped
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            *slot = Some(description(error));
        }
    }
);

impl StreamHandler {
    fn new(frames: SyncSender<Frame>, stopped: Arc<Mutex<Option<String>>>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(HandlerIvars { frames, stopped });
        // SAFETY: NSObject's `init` accepts a fully initialized instance.
        unsafe { msg_send![super(this), init] }
    }
}

#[cfg(test)]
mod tests;
