#[cfg(target_os = "windows")]
use crate::WindowsScreenCaptureFrame;
use crate::{
    App, Bounds, DevicePixels, Element, ElementId, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, ObjectFit, Pixels, Size, Style, StyleRefinement, Styled, Window,
};
#[cfg(target_os = "macos")]
use core_video::pixel_buffer::CVPixelBuffer;
#[cfg(target_os = "macos")]
use metal::SharedEvent;
use refineable::Refineable;
#[cfg(target_os = "macos")]
use std::sync::Arc;

/// A source of a surface's content.
#[derive(Clone)]
pub enum SurfaceSource {
    /// A macOS image buffer from CoreVideo
    #[cfg(target_os = "macos")]
    Surface(CVPixelBuffer),
    /// A BGRA Metal surface produced outside GPUI.
    #[cfg(target_os = "macos")]
    ExternalMetal(ExternalMetalSurface),
    /// A GPU texture handle (type-erased to avoid depending on wgpu)
    #[cfg(any(
        target_os = "linux",
        target_os = "freebsd",
        all(target_os = "macos", feature = "custom-gpu"),
        all(target_family = "wasm", feature = "custom-gpu")
    ))]
    Texture {
        /// The GPU texture, type-erased (expected to be `Arc<wgpu::Texture>`)
        #[cfg(not(target_family = "wasm"))]
        texture: std::sync::Arc<dyn std::any::Any + Send + Sync>,
        /// The GPU texture, type-erased (expected to be `Arc<wgpu::Texture>`).
        ///
        /// WGPU handles are intentionally thread-local in browser builds.
        #[cfg(target_family = "wasm")]
        texture: std::sync::Arc<dyn std::any::Any>,
        /// Dimensions of the texture in device pixels
        size: Size<DevicePixels>,
    },
    /// A native Windows Graphics Capture texture.
    #[cfg(target_os = "windows")]
    WindowsCapture(WindowsScreenCaptureFrame),
    /// A placeholder for platforms that cannot import native surfaces.
    #[doc(hidden)]
    Unsupported(Size<DevicePixels>),
}

impl std::fmt::Debug for SurfaceSource {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            #[cfg(target_os = "macos")]
            SurfaceSource::Surface(ref buf) => _f.debug_tuple("Surface").field(buf).finish(),
            #[cfg(target_os = "macos")]
            SurfaceSource::ExternalMetal(ref surface) => _f
                .debug_tuple("ExternalMetal")
                .field(&surface.descriptor)
                .finish(),
            #[cfg(any(
                target_os = "linux",
                target_os = "freebsd",
                all(target_os = "macos", feature = "custom-gpu"),
                all(target_family = "wasm", feature = "custom-gpu")
            ))]
            SurfaceSource::Texture { size, .. } => _f
                .debug_struct("Texture")
                .field("size", &size)
                .finish_non_exhaustive(),
            #[cfg(target_os = "windows")]
            SurfaceSource::WindowsCapture(ref frame) => frame.fmt(_f),
            SurfaceSource::Unsupported(size) => _f.debug_tuple("Unsupported").field(&size).finish(),
        }
    }
}

impl SurfaceSource {
    fn size(&self) -> Size<DevicePixels> {
        match self {
            #[cfg(target_os = "macos")]
            SurfaceSource::Surface(buffer) => {
                crate::size(buffer.get_width().into(), buffer.get_height().into())
            }
            #[cfg(target_os = "macos")]
            SurfaceSource::ExternalMetal(surface) => surface.descriptor.size,
            #[cfg(any(
                target_os = "linux",
                target_os = "freebsd",
                all(target_os = "macos", feature = "custom-gpu"),
                all(target_family = "wasm", feature = "custom-gpu")
            ))]
            SurfaceSource::Texture { size, .. } => *size,
            #[cfg(target_os = "windows")]
            SurfaceSource::WindowsCapture(frame) => frame.size(),
            SurfaceSource::Unsupported(size) => *size,
        }
    }
}

/// Identity used by a Metal renderer to cache an external texture.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ExternalTextureIdentity {
    /// Caller-provided resource identity.
    pub resource_id: u64,
    /// Resource generation, changed when dimensions or format change.
    pub generation: u64,
    /// Identity of the actual IOSurface backing.
    pub iosurface_id: u32,
}

/// Properties of an externally-produced Metal surface.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExternalSurfaceDescriptor {
    /// Identity, including the actual IOSurface ID.
    pub identity: ExternalTextureIdentity,
    /// Dimensions in physical pixels.
    pub size: Size<DevicePixels>,
    /// CoreVideo/Metal pixel-format code.
    pub pixel_format: u32,
}

/// Producer synchronization encoded in the command buffer that samples a
/// surface.
#[cfg(target_os = "macos")]
#[derive(Clone)]
pub struct MetalSharedEventWait {
    /// Event signaled after the producer finishes writing the surface.
    pub event: SharedEvent,
    /// Required producer signal value.
    pub value: u64,
}

/// A generic external BGRA surface for the macOS Metal renderer.
#[cfg(target_os = "macos")]
#[derive(Clone)]
pub struct ExternalMetalSurface {
    /// Surface descriptor.
    pub descriptor: ExternalSurfaceDescriptor,
    /// IOSurface-backed CoreVideo image.
    pub image_buffer: CVPixelBuffer,
    /// Optional producer fence.
    pub wait: Option<MetalSharedEventWait>,
    /// Called immediately before a command buffer containing this surface is submitted.
    pub on_gpu_submitted: Arc<dyn Fn() + Send + Sync>,
    /// Called after the GPU completes a command buffer sampling this resource.
    pub on_gpu_complete: Arc<dyn Fn() + Send + Sync>,
}

#[cfg(target_os = "macos")]
impl std::fmt::Debug for MetalSharedEventWait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetalSharedEventWait")
            .field("value", &self.value)
            .finish_non_exhaustive()
    }
}

#[cfg(target_os = "macos")]
impl std::fmt::Debug for ExternalMetalSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalMetalSurface")
            .field("descriptor", &self.descriptor)
            .field("wait", &self.wait)
            .finish_non_exhaustive()
    }
}

#[cfg(target_os = "macos")]
impl ExternalMetalSurface {
    /// Creates an external surface with an optional producer fence.
    pub fn new(
        descriptor: ExternalSurfaceDescriptor,
        image_buffer: CVPixelBuffer,
        wait: Option<MetalSharedEventWait>,
        on_gpu_submitted: impl Fn() + Send + Sync + 'static,
        on_gpu_complete: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        assert_ne!(descriptor.identity.resource_id, 0);
        assert_ne!(descriptor.identity.generation, 0);
        assert!(descriptor.size.width.0 > 0 && descriptor.size.height.0 > 0);
        assert_eq!(descriptor.size.width.0 as usize, image_buffer.get_width());
        assert_eq!(descriptor.size.height.0 as usize, image_buffer.get_height());
        use core_foundation::base::TCFType;
        use core_video::pixel_buffer_io_surface::CVPixelBufferGetIOSurface;
        let surface = unsafe { CVPixelBufferGetIOSurface(image_buffer.as_concrete_TypeRef()) };
        assert!(
            !surface.is_null(),
            "external image has no IOSurface backing"
        );
        #[allow(deprecated)]
        let actual_surface_id = unsafe { io_surface::IOSurfaceGetID(surface) };
        assert_ne!(actual_surface_id, 0, "external IOSurface has an invalid ID");
        assert_eq!(
            descriptor.identity.iosurface_id, actual_surface_id,
            "external texture identity must contain the actual IOSurface ID"
        );
        Self {
            descriptor,
            image_buffer,
            wait,
            on_gpu_submitted: Arc::new(on_gpu_submitted),
            on_gpu_complete: Arc::new(on_gpu_complete),
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::ExternalTextureIdentity;

    #[test]
    fn iosurface_identity_distinguishes_reused_logical_ids() {
        let first = ExternalTextureIdentity {
            resource_id: 7,
            generation: 3,
            iosurface_id: 101,
        };
        let replacement = ExternalTextureIdentity {
            iosurface_id: 102,
            ..first
        };
        assert_ne!(first, replacement);
        assert_ne!(
            std::collections::HashSet::from([first]),
            std::collections::HashSet::from([replacement])
        );
    }
}

#[cfg(target_os = "macos")]
impl From<CVPixelBuffer> for SurfaceSource {
    fn from(value: CVPixelBuffer) -> Self {
        SurfaceSource::Surface(value)
    }
}

#[cfg(target_os = "windows")]
impl From<WindowsScreenCaptureFrame> for SurfaceSource {
    fn from(value: WindowsScreenCaptureFrame) -> Self {
        SurfaceSource::WindowsCapture(value)
    }
}

#[cfg(all(target_os = "windows", feature = "screen-capture"))]
impl From<crate::ScreenCaptureFrame> for SurfaceSource {
    fn from(value: crate::ScreenCaptureFrame) -> Self {
        SurfaceSource::WindowsCapture(value.0)
    }
}

/// A surface element.
pub struct Surface {
    source: SurfaceSource,
    object_fit: ObjectFit,
    style: StyleRefinement,
}

/// Create a new surface element.
pub fn surface(source: impl Into<SurfaceSource>) -> Surface {
    Surface {
        source: source.into(),
        object_fit: ObjectFit::Contain,
        style: Default::default(),
    }
}

impl Surface {
    /// Set the object fit for the image.
    pub fn object_fit(mut self, object_fit: ObjectFit) -> Self {
        self.object_fit = object_fit;
        self
    }
}

impl Element for Surface {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.refine(&self.style);
        let layout_id = window.request_layout(style, [], cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        _window: &mut Window,
        _: &mut App,
    ) {
        let new_bounds = self.object_fit.get_bounds(_bounds, self.source.size());
        let mut style = Style::default();
        style.refine(&self.style);
        let corner_radii = style.corner_radii.to_pixels(_window.rem_size());
        _window.with_element_opacity(style.opacity, |window| {
            window.paint_surface(new_bounds, self.source.clone(), corner_radii);
        });
    }
}

impl IntoElement for Surface {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Styled for Surface {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}
