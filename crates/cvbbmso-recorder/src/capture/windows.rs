//! Desktop Duplication capture of the primary display.

use std::marker::PhantomData;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_MODE_ROTATION_ROTATE90, DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter,
    IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::core::{Error as WindowsError, Interface, Result as WindowsResult};

use super::{Frame, Rotation, rotate};
use crate::{CaptureFailure, PlatformError};

/// Bytes per pixel of the desktop duplication surface.
const BYTES_PER_PIXEL: usize = 4;

/// Duplication of the primary display output.
pub(crate) struct Desktop {
    duplication: IDXGIOutputDuplication,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    rotation: Rotation,
    // Declared last so the apartment outlives the interfaces created in it.
    _com: ComGuard,
    // The duplication and its COM apartment are bound to one thread; moving the
    // backend elsewhere would run `CoUninitialize` on the wrong thread.
    _thread: PhantomData<*const ()>,
}

impl Desktop {
    /// Opens the primary display for duplication.
    ///
    /// The backend must stay on the thread that created it: it holds the COM
    /// apartment initialised by [`Desktop::open`].
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::Failed`] when the display cannot be duplicated.
    pub(crate) fn open() -> Result<Self, PlatformError> {
        Self::open_inner().map_err(|error| failure(&error))
    }

    /// Waits for the next changed frame.
    ///
    /// Returns `Ok(None)` when the desktop did not change within `timeout_ms`,
    /// which is a normal condition for a still screen rather than a fault.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::Failed`] when the desktop can no longer be read.
    pub(crate) fn next_frame(&mut self, timeout_ms: u32) -> Result<Option<Frame>, PlatformError> {
        self.next_frame_inner(timeout_ms)
            .map_err(|error| failure(&error))
    }

    fn open_inner() -> WindowsResult<Self> {
        // SAFETY: every call follows the Desktop Duplication sequence; the
        // apartment guard is stored with the interfaces it created.
        unsafe {
            let com = ComGuard::initialize()?;
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let adapter: IDXGIAdapter = factory.EnumAdapters1(0)?.cast()?;
            let output = adapter.EnumOutputs(0)?;
            let output1: IDXGIOutput1 = output.cast()?;

            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                Some(&adapter),
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&raw mut device),
                None,
                Some(&raw mut context),
            )?;

            let device = device.ok_or_else(WindowsError::from_win32)?;
            let context = context.ok_or_else(WindowsError::from_win32)?;
            let duplication = output1.DuplicateOutput(&device)?;
            let rotation = match duplication.GetDesc().Rotation {
                DXGI_MODE_ROTATION_ROTATE90 => Rotation::Clockwise90,
                DXGI_MODE_ROTATION_ROTATE180 => Rotation::Half,
                DXGI_MODE_ROTATION_ROTATE270 => Rotation::Clockwise270,
                _ => Rotation::None,
            };

            Ok(Self {
                duplication,
                device,
                context,
                rotation,
                _com: com,
                _thread: PhantomData,
            })
        }
    }

    fn next_frame_inner(&mut self, timeout_ms: u32) -> WindowsResult<Option<Frame>> {
        // SAFETY: the guards below release the desktop frame and the staging
        // mapping on every exit path, including the early returns.
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            match self
                .duplication
                .AcquireNextFrame(timeout_ms, &raw mut info, &raw mut resource)
            {
                Ok(()) => {}
                Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
                Err(error) => return Err(error),
            }
            let _frame = FrameGuard(&self.duplication);

            let resource = resource.ok_or_else(WindowsError::from_win32)?;
            let texture: ID3D11Texture2D = resource.cast()?;

            let mut description = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&raw mut description);
            description.Usage = D3D11_USAGE_STAGING;
            description.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            description.BindFlags = 0;
            description.MiscFlags = 0;
            description.MipLevels = 1;
            description.ArraySize = 1;

            let mut staging: Option<ID3D11Texture2D> = None;
            self.device
                .CreateTexture2D(&raw const description, None, Some(&raw mut staging))?;
            let staging = staging.ok_or_else(WindowsError::from_win32)?;
            self.context.CopyResource(&staging, &texture);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped))?;
            let _mapping = MapGuard {
                context: &self.context,
                texture: &staging,
            };

            let packed = copy_rows(&mapped, description.Width, description.Height);
            let (pixels, width, height) = rotate(
                &packed,
                description.Width,
                description.Height,
                self.rotation,
            );
            Ok(Some(Frame {
                width,
                height,
                pixels,
            }))
        }
    }
}

/// Keeps the calling thread's COM apartment initialised.
struct ComGuard;

impl ComGuard {
    fn initialize() -> WindowsResult<Self> {
        // SAFETY: a successful call is matched once by `Drop`.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
        Ok(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        // SAFETY: every successful `CoInitializeEx` is balanced here, on the same
        // thread, after the interfaces created in the apartment are released.
        unsafe { CoUninitialize() };
    }
}

/// Releases the desktop frame held by a successful `AcquireNextFrame`.
struct FrameGuard<'a>(&'a IDXGIOutputDuplication);

impl Drop for FrameGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: the guard exists only after a successful acquisition, and DXGI
        // requires the frame to be released before the next acquisition.
        let _ = unsafe { self.0.ReleaseFrame() };
    }
}

/// Unmaps the staging texture after a successful `Map`.
struct MapGuard<'a> {
    context: &'a ID3D11DeviceContext,
    texture: &'a ID3D11Texture2D,
}

impl Drop for MapGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: the guard exists only after a successful mapping of this texture.
        unsafe { self.context.Unmap(self.texture, 0) };
    }
}

/// Wraps a platform error so the caller keeps its code and cause.
fn failure(error: &WindowsError) -> PlatformError {
    PlatformError::Failed(CaptureFailure::from_windows(error))
}

/// Copies a mapped surface into tightly packed rows.
fn copy_rows(mapped: &D3D11_MAPPED_SUBRESOURCE, width: u32, height: u32) -> Vec<u8> {
    let row_bytes = usize::try_from(width).unwrap_or(0) * BYTES_PER_PIXEL;
    let rows = usize::try_from(height).unwrap_or(0);
    let pitch = usize::try_from(mapped.RowPitch).unwrap_or(0);
    let mut pixels = Vec::with_capacity(row_bytes * rows);
    let base = mapped.pData.cast::<u8>();
    for row in 0..rows {
        // SAFETY: the surface holds `rows` rows of at least `row_bytes` bytes, and
        // `row_bytes` never exceeds the row pitch reported by the mapping.
        let bytes = unsafe { std::slice::from_raw_parts(base.add(row * pitch), row_bytes) };
        pixels.extend_from_slice(bytes);
    }
    pixels
}

#[cfg(test)]
mod tests;
