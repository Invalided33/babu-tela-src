//! My own tile without a trip through memory. The screen share's preview
//! (hw.rs) is drawn on the graphics card into textures that egui's
//! Direct3D 12 device opens as well, through NT handles, and egui draws
//! them as they are: nothing is read back, converted or uploaded. Both
//! devices must be on the same card and egui must draw with Direct3D 12
//! (main.rs `wgpu_options`); otherwise hw.rs reads the preview back into a
//! ColorImage, as before.
//!
//! The pictures go into a small ring of textures in turn. Two shared fences
//! keep the devices apart: the sharing side signals `ready` once a picture
//! is in its texture (egui's queue waits for that value before drawing,
//! when it has not been reached yet), and egui's queue signals `done` with
//! the number of the last frame whose drawing finished, which the sharing
//! side checks before writing over a texture a frame showed.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use eframe::egui_wgpu::RenderState;
use eframe::wgpu;
use egui::load::SizedTexture;
use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
use windows::Win32::Graphics::Direct3D12::{ID3D12Device, ID3D12Fence, ID3D12Resource};

/// Textures in a ring: one being drawn by egui, one just written, one free.
pub const RING: usize = 3;

static RENDER: OnceLock<RenderState> = OnceLock::new();

/// Called once, as the window opens, with what egui draws with.
pub fn install(render: Option<&RenderState>) {
    if let Some(render) = render {
        let _ = RENDER.set(render.clone());
    }
}

/// The card egui draws on, when it draws with Direct3D 12.
pub fn card() -> Option<LUID> {
    static CARD: OnceLock<Option<LUID>> = OnceLock::new();
    *CARD.get_or_init(|| {
        let device = raw_device(RENDER.get()?)?;
        // SAFETY: a plain query.
        Some(unsafe { device.GetAdapterLuid() })
    })
}

/// egui's Direct3D 12 device, when that is what it draws with.
fn raw_device(render: &RenderState) -> Option<ID3D12Device> {
    // SAFETY: the device is only used to open shared objects, which then
    // belong to wgpu as textures of this same device, and to read its card.
    let hal = unsafe { render.device.as_hal::<wgpu::hal::api::Dx12>() }?;
    Some(hal.raw_device().clone())
}

/// An NT handle to a shared texture or fence, closed when dropped.
pub struct Handle(pub HANDLE);

// SAFETY: a handle is a number that means the same in every thread of the
// process; this one is closed once, when dropped.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle was made for this object and is closed once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// A ring of textures the sharing side draws my tile into, and the fences
/// between it and egui.
pub struct Ring {
    /// Tells rings apart (a new size makes a new one).
    pub id: u64,
    pub size: [usize; 2],
    pub textures: [Handle; RING],
    /// Signalled by the sharing side once a picture is written.
    pub ready: Handle,
    /// Signalled by egui's queue with the last frame finished.
    pub done: Handle,
    /// The last frame that showed each texture: it is written again only
    /// once `done` has reached that.
    pub shown_in: [AtomicU64; RING],
    /// egui's device could not open the ring: the sharing side reads its
    /// preview back instead.
    pub refused: AtomicBool,
}

/// One picture: which texture of which ring, and the `ready` value that
/// says it is written.
pub struct Frame {
    pub ring: Arc<Ring>,
    pub slot: usize,
    pub ready: u64,
}

/// What egui's device made of a ring: a texture id for each of its
/// textures, and the fences.
struct Opened {
    ring: Arc<Ring>,
    _textures: Vec<wgpu::Texture>,
    ids: Vec<egui::TextureId>,
    ready: ID3D12Fence,
    done: ID3D12Fence,
}

impl Opened {
    fn open(render: &RenderState, ring: Arc<Ring>) -> Result<Opened, String> {
        use wgpu::hal::api::Dx12;
        let device = raw_device(render).ok_or("o egui não desenha com Direct3D 12")?;
        let [w, h] = ring.size;
        let size = wgpu::Extent3d {
            width: w as u32,
            height: h as u32,
            depth_or_array_layers: 1,
        };
        let desc = wgpu::TextureDescriptor {
            label: Some("prévia na placa"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // What hw.rs draws, in the screen's own (gamma) values, as
            // egui keeps its own textures.
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };
        let mut opened = Opened {
            ring: ring.clone(),
            _textures: Vec::with_capacity(RING),
            ids: Vec::with_capacity(RING),
            ready: open_fence(&device, &ring.ready)?,
            done: open_fence(&device, &ring.done)?,
        };
        for handle in &ring.textures {
            let mut resource: Option<ID3D12Resource> = None;
            // SAFETY: the handle is a shared texture's, made by the sharing
            // side for this device's card.
            let opening = unsafe { device.OpenSharedHandle(handle.0, &mut resource) };
            let Some(resource) = opening.ok().and(resource) else {
                opened.free(render);
                return Err("a textura compartilhada não abriu".into());
            };
            // SAFETY: the resource is a BGRA texture of `size`, with one
            // mip and one sample, on this device's card. wgpu takes it as
            // not yet written (the COMMON state a shared texture opens in).
            // A Direct3D 11 shared texture opens here with simultaneous
            // access (measured: ALLOW_RENDER_TARGET | ALLOW_SIMULTANEOUS_
            // ACCESS), so it decays back to COMMON after every submission
            // and the other device needs no hand-over of its state.
            let texture = unsafe {
                let hal = wgpu::hal::dx12::Device::texture_from_raw(
                    resource,
                    desc.format,
                    desc.dimension,
                    size,
                    1,
                    1,
                );
                render.device.create_texture_from_hal::<Dx12>(
                    hal,
                    &desc,
                    wgpu::wgt::TextureUses::UNINITIALIZED,
                )
            };
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let id = render.renderer.write().register_native_texture(
                &render.device,
                &view,
                wgpu::FilterMode::Linear,
            );
            opened.ids.push(id);
            opened._textures.push(texture);
        }
        Ok(opened)
    }

    fn free(&self, render: &RenderState) {
        let mut renderer = render.renderer.write();
        for id in &self.ids {
            renderer.free_texture(id);
        }
    }

    /// Stages what egui's next submission needs to show `frame`, and says
    /// which texture to draw.
    fn show(&self, render: &RenderState, frame: &Frame, frame_nr: u64) -> Option<SizedTexture> {
        use wgpu::hal::api::Dx12;
        let id = *self.ids.get(frame.slot)?;
        {
            // SAFETY: only fence operations staged on egui's own queue,
            // which wgpu-hal offers for exactly this.
            let queue = unsafe { render.queue.as_hal::<Dx12>() }?;
            // SAFETY: a plain query.
            if unsafe { self.ready.GetCompletedValue() } < frame.ready {
                queue.add_wait_fence(self.ready.clone(), frame.ready);
            }
            // After the next submission (this frame's, or one before it),
            // everything submitted until the last frame is done: the
            // textures shown until then may be written again.
            queue.add_signal_fence(self.done.clone(), frame_nr.saturating_sub(1));
        }
        self.ring.shown_in[frame.slot].fetch_max(frame_nr, Ordering::Relaxed);
        let [w, h] = self.ring.size;
        Some(SizedTexture::new(id, egui::vec2(w as f32, h as f32)))
    }
}

fn open_fence(device: &ID3D12Device, handle: &Handle) -> Result<ID3D12Fence, String> {
    let mut fence: Option<ID3D12Fence> = None;
    // SAFETY: the handle is a shared fence's, made by the sharing side.
    unsafe { device.OpenSharedHandle(handle.0, &mut fence) }.map_err(|e| e.to_string())?;
    fence.ok_or_else(|| "a fence compartilhada não abriu".to_owned())
}

/// Someone's pictures on the card, as the call view draws them.
#[derive(Default)]
pub struct Tile {
    newest: Option<Frame>,
    shown: Option<Frame>,
    opened: Option<Opened>,
}

impl Tile {
    pub fn put(&mut self, frame: Frame) {
        self.newest = Some(frame);
    }

    /// The texture to draw in frame `frame_nr` (`egui::Context::
    /// cumulative_frame_nr`), the newest picture's when one came.
    pub fn frame(&mut self, frame_nr: u64) -> Option<SizedTexture> {
        let render = RENDER.get()?;
        if let Some(newest) = self.newest.take() {
            if self
                .opened
                .as_ref()
                .is_none_or(|o| o.ring.id != newest.ring.id)
            {
                if let Some(old) = self.opened.take() {
                    old.free(render);
                }
                match Opened::open(render, newest.ring.clone()) {
                    Ok(opened) => self.opened = Some(opened),
                    Err(e) => {
                        crate::logln!("vídeo: a prévia na placa não abriu ({e}), vai pela memória");
                        newest.ring.refused.store(true, Ordering::Relaxed);
                        self.shown = None;
                        return None;
                    }
                }
            }
            self.shown = Some(newest);
        }
        let (opened, shown) = (self.opened.as_ref()?, self.shown.as_ref()?);
        opened.show(render, shown, frame_nr)
    }

    /// Lets egui's textures go (the tile is gone).
    pub fn free(self) {
        if let (Some(render), Some(opened)) = (RENDER.get(), self.opened) {
            opened.free(render);
        }
    }
}
