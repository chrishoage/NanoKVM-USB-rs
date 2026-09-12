//! GPU presentation on a dedicated thread.
//!
//! Frames are presented at capture cadence using a supported presentation mode. Surface
//! recovery and slow compositor callbacks do not block the input event loop. UI texture
//! deltas are accumulated even when an intermediate UI frame is skipped.
//!
//! GPU objects are retained until process exit: a detached render thread could otherwise
//! run EGL destructors after winit has closed the Wayland connection.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use winit::window::Window;

use crate::capture::v4l2::now_monotonic;
use crate::capture::{DecodedFrame, Slot};
use crate::viewer::chrome::frame::{ChromeFrame, ChromeReceiver};

/// Maximum wait for a pending frame before servicing render control state.
const FRAME_WAIT: Duration = Duration::from_millis(100);

/// Timing samples kept per metric. At 60 fps this is about eight seconds of history, comfortably
/// more than the longest `--stats-interval` anyone is likely to use, and the ring is drained on
/// every report anyway.
const SAMPLE_CAPACITY: usize = 512;

/// The clear colour before any frame has ever arrived. a neutral dark grey and
/// not the words "no signal": this hardware cannot report signal state and pixel
/// content is never evidence of it.
const EMPTY_CLEAR: wgpu::Color = wgpu::Color {
    r: 0.05,
    g: 0.05,
    b: 0.06,
    a: 1.0,
};

/// Recover a mutex guard rather than panicking on poison. See the module docs.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A rectangle in physical pixels, with its origin at the window's top-left.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Aspect-preserving video rectangle in physical window pixels.
/// Returns `None` for a zero-sized window or frame, where drawing and mapping are invalid.
pub fn letterbox(window: (u32, u32), frame: (u32, u32)) -> Option<Rect> {
    if window.0 == 0 || window.1 == 0 || frame.0 == 0 || frame.1 == 0 {
        return None;
    }
    let (ww, wh) = (f64::from(window.0), f64::from(window.1));
    let (fw, fh) = (f64::from(frame.0), f64::from(frame.1));
    // Scale to the tighter of the two axes; the other one gets the bars.
    let scale = (ww / fw).min(wh / fh);
    let w = fw * scale;
    let h = fh * scale;
    // Clamped, not merely computed. `fw * (ww / fw)` is `ww` only in exact arithmetic; in binary
    // floating point it can land a few ulps either side, which yields a bar of about -1e-12 px and
    // a rectangle that starts fractionally outside the window. `set_viewport` rejects a viewport
    // that is not inside the attachment, and wgpu's validation error is a panic on the render
    // thread — so the geometry is made true rather than nearly true.
    let x = ((ww - w) / 2.0).max(0.0);
    let y = ((wh - h) / 2.0).max(0.0);
    Some(Rect {
        x: x as f32,
        y: y as f32,
        width: w.min(ww - x) as f32,
        height: h.min(wh - y) as f32,
    })
}

/// Per-interval render timings. `age_*` measures monotonic capture timestamp to
/// GPU submission; it does not establish when the display showed the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RenderStats {
    /// Frames presented during the interval.
    pub presented: u64,
    /// Frames whose acquire returned `Outdated`/`Lost` and were skipped after reconfiguring.
    pub surface_recoveries: u64,
    /// Median duration of `SurfaceTexture::present()`, in microseconds. It can block
    /// when compositor pacing or visibility prevents presentation.
    pub present_p50_us: u64,
    /// Longest `present()` in the interval.
    pub present_max_us: u64,
    /// Median capture-to-submit age, microseconds.
    pub age_p50_us: u64,
    /// Longest capture-to-submit age in the interval.
    pub age_max_us: u64,
    /// Median UI draw duration, in microseconds, covering texture updates, buffer
    /// updates, and rendering. Samples are taken only when UI geometry is drawn.
    pub egui_p50_us: u64,
    /// Longest chrome draw in the interval.
    pub egui_max_us: u64,
}

#[derive(Default)]
struct StatsInner {
    present_us: VecDeque<u64>,
    age_us: VecDeque<u64>,
    egui_us: VecDeque<u64>,
    presented: u64,
    surface_recoveries: u64,
}

impl StatsInner {
    fn push(samples: &mut VecDeque<u64>, v: u64) {
        if samples.len() == SAMPLE_CAPACITY {
            samples.pop_front();
        }
        samples.push_back(v);
    }
}

/// Percentile of an unsorted sample set, by nearest rank. Empty input is 0.
fn percentile(samples: &mut [u64], p: f64) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    let i = ((samples.len() - 1) as f64 * p).round() as usize;
    samples[i]
}

/// Everything the event loop and the render thread share.
///
/// Both directions are one small mutex each rather than a channel: the render thread
/// must never block on the event loop, and the event loop must never block on a thread that spends
/// 99 % of its time inside `present()`.
#[derive(Debug, Default)]
pub struct RenderShared {
    /// Set by the event loop on `WindowEvent::Resized`; consumed by the render thread, which owns
    /// surface reconfiguration.
    resize: Mutex<Option<(u32, u32)>>,
    /// The decoded frame's own dimensions, published by the render thread whenever they change.
    ///
    /// Only the dimensions. The letterbox rectangle is computed on the event loop instead
    /// : the event loop learns the new window size from `Resized` immediately, while a
    /// rectangle published from here is up to a frame-wait old — and a cursor mapped through a
    /// stale rectangle lands on the wrong pixel of a live console. Frame dimensions do not have
    /// that problem: they come from the JPEG header rather than `G_FMT` and change only
    /// when the target's resolution does.
    frame: Mutex<Option<(u32, u32)>>,
    stats: Mutex<StatsInner>,
    stop: AtomicBool,
    /// Set by the event loop from `WindowEvent::Occluded`. While occluded the render thread must
    /// not present: a compositor that is not scheduling frame callbacks for this surface leaves
    /// `present()` blocked indefinitely, and teardown then joins a thread that never returns
    /// (measured on a locked session: `present p50 999 ms`).
    occluded: AtomicBool,
    /// Raised by the render thread as it leaves, so [`RenderShared::wait_finished`] can bound the
    /// join. Paired with `wake`.
    finished: Mutex<bool>,
    wake: Condvar,
    /// A failure the render thread cannot continue past, for the event loop to report and exit on.
    fatal: Mutex<Option<String>>,
}

impl std::fmt::Debug for StatsInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StatsInner")
            .field("presented", &self.presented)
            .field("samples", &self.present_us.len())
            .finish()
    }
}

impl RenderShared {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the render thread to reconfigure the surface. Called from the event loop.
    pub fn request_resize(&self, width: u32, height: u32) {
        *lock(&self.resize) = Some((width, height));
    }

    /// The decoded frame's dimensions, or `None` before the first frame. The event loop turns
    /// these plus its own window size into the video rectangle.
    pub fn frame_size(&self) -> Option<(u32, u32)> {
        *lock(&self.frame)
    }

    /// Tell the render thread whether the compositor is showing this surface.
    ///
    /// `true` means "do not present": see the field's documentation. Frames keep being taken
    /// while occluded, so the pipeline's one-pending-frame rule still holds and the
    /// renderer resumes with a current image rather than a backlog.
    pub fn set_occluded(&self, occluded: bool) {
        self.occluded.store(occluded, Ordering::SeqCst);
    }

    /// Whether the event loop last said the surface was occluded.
    pub fn is_occluded(&self) -> bool {
        self.occluded.load(Ordering::SeqCst)
    }

    /// Wait up to `timeout` for the render thread to leave its loop; `true` if it has.
    ///
    /// The event loop uses this to bound its join. Joining unconditionally is what hangs a
    /// shutdown when the thread is parked inside `present()` on a surface the compositor is not
    /// scheduling: the release-all has already been requested by then and `main`'s
    /// `writer.shutdown()` must still run, so a renderer that will not come back is detached and
    /// reported rather than waited for.
    pub fn wait_finished(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut done = lock(&self.finished);
        while !*done {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (guard, _) = self
                .wake
                .wait_timeout(done, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            done = guard;
        }
        true
    }

    /// Raise the finished flag. Called from the render thread's exit guard, panic or not.
    fn mark_finished(&self) {
        *lock(&self.finished) = true;
        self.wake.notify_all();
    }

    /// Drain the interval's timings. Counters reset; the sample rings are emptied.
    pub fn take_stats(&self) -> RenderStats {
        let mut s = lock(&self.stats);
        let mut present: Vec<u64> = s.present_us.drain(..).collect();
        let mut age: Vec<u64> = s.age_us.drain(..).collect();
        let mut egui: Vec<u64> = s.egui_us.drain(..).collect();
        let out = RenderStats {
            presented: s.presented,
            surface_recoveries: s.surface_recoveries,
            present_p50_us: percentile(&mut present, 0.50),
            present_max_us: present.iter().copied().max().unwrap_or(0),
            age_p50_us: percentile(&mut age, 0.50),
            age_max_us: age.iter().copied().max().unwrap_or(0),
            egui_p50_us: percentile(&mut egui, 0.50),
            egui_max_us: egui.iter().copied().max().unwrap_or(0),
        };
        s.presented = 0;
        s.surface_recoveries = 0;
        out
    }

    /// The render thread's fatal error, if it hit one.
    pub fn fatal(&self) -> Option<String> {
        lock(&self.fatal).clone()
    }

    /// Request render shutdown. An idle worker wakes within `FRAME_WAIT`;
    /// a worker blocked in presentation cannot observe the flag until that call returns.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// The wgpu objects, created on the event loop thread and then owned by the render thread.
pub struct Gpu {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// `None` until the first frame arrives. Recreated whenever the frame dimensions change,
    /// which they can at any time.
    texture: Option<VideoTexture>,
    /// The chrome's renderer. This thread owns only the renderer: the
    /// `egui::Context`, the UI build and the tessellation are on the event loop, because the
    /// routing rule needs the chrome's state synchronously while an input event is in hand
    /// .
    egui: egui_wgpu::Renderer,
}

struct VideoTexture {
    texture: wgpu::Texture,
    width: u32,
    height: u32,
    bind_group: wgpu::BindGroup,
}

/// One textured quad, letterboxed by the render pass viewport rather than by a transform, so the
/// shader has no uniforms and nothing has to be uploaded per frame besides the image itself.
const SHADER: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    // One oversized triangle covering the viewport.
    var xy = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    let p = xy[i];
    var out: VsOut;
    out.pos = vec4<f32>(p, 0.0, 1.0);
    // Row 0 of the RGBA frame is the top row, which is clip-space y = +1.
    out.uv = vec2<f32>((p.x + 1.0) * 0.5, (1.0 - p.y) * 0.5);
    return out;
}

@group(0) @binding(0) var video: texture_2d<f32>;
@group(0) @binding(1) var video_sampler: sampler;

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(video, video_sampler, in.uv);
}
"#;

/// Names for logging supported presentation modes.
fn mode_name(m: wgpu::PresentMode) -> &'static str {
    match m {
        wgpu::PresentMode::AutoVsync => "AutoVsync",
        wgpu::PresentMode::AutoNoVsync => "AutoNoVsync",
        wgpu::PresentMode::Fifo => "Fifo",
        wgpu::PresentMode::FifoRelaxed => "FifoRelaxed",
        wgpu::PresentMode::Immediate => "Immediate",
        wgpu::PresentMode::Mailbox => "Mailbox",
    }
}

/// Prefer Fifo from supported modes; otherwise use the first offered mode.
/// Requesting an unsupported mode can fail surface configuration.
fn choose_present_mode(offered: &[wgpu::PresentMode]) -> Option<wgpu::PresentMode> {
    if offered.contains(&wgpu::PresentMode::Fifo) {
        return Some(wgpu::PresentMode::Fifo);
    }
    offered.first().copied()
}

impl Gpu {
    /// Create the instance, adapter, device, surface and pipeline for `window`.
    ///
    /// Runs on the event loop thread, once, before the render thread starts.
    ///
    /// # Errors
    ///
    /// No adapter, or a device that will not open, is reported as an error naming the likely
    /// cause rather than panicking: on this platform it means
    /// no Vulkan or GL driver could be loaded.
    pub fn create(window: Arc<Window>) -> anyhow::Result<Gpu> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let surface = instance
            .create_surface(Arc::clone(&window))
            .context("creating a wgpu surface for the window")?;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .map_err(|e| {
            anyhow!(
                "no usable GPU adapter ({e}). The most likely cause is that no Vulkan or GL \
                 driver could be loaded for this display: check that a Mesa or vendor driver is \
                 installed and that libvulkan/libEGL are present."
            )
        })?;
        let info = adapter.get_info();
        log::info!(
            "gpu adapter: {} ({:?}, {:?}) driver {} {}",
            info.name,
            info.backend,
            info.device_type,
            info.driver,
            info.driver_info
        );

        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .map_err(|e| {
                    anyhow!(
                        "the GPU adapter '{}' would not open a device ({e}). The most likely \
                         cause is a driver that loaded but cannot service this process.",
                        info.name
                    )
                })?;

        let caps = surface.get_capabilities(&adapter);
        // Select only from modes the surface reports as supported.
        log::info!(
            "surface present modes offered: {:?}",
            caps.present_modes
                .iter()
                .copied()
                .map(mode_name)
                .collect::<Vec<_>>()
        );
        let present_mode = choose_present_mode(&caps.present_modes).ok_or_else(|| {
            anyhow!("the surface offers no present modes at all; the driver cannot present here")
        })?;
        log::info!("present mode chosen: {}", mode_name(present_mode));

        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .or_else(|| caps.formats.first().copied())
            .ok_or_else(|| anyhow!("the surface offers no texture formats"))?;
        let alpha_mode = caps
            .alpha_modes
            .first()
            .copied()
            .unwrap_or(wgpu::CompositeAlphaMode::Auto);

        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("nanokvm-video"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(SHADER)),
        });
        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("nanokvm-video-bind-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("nanokvm-video-layout"),
            bind_group_layouts: &[&bind_layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("nanokvm-video-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(format.into())],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        // Linear: the video is scaled to an arbitrary window size, and nearest sampling of a
        // downscaled 1080p desktop aliases text badly.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("nanokvm-video-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        // sRGB is handled for us: `Renderer::new` selects its gamma or linear framebuffer shader
        // from `output_color_format.is_srgb()`, and `format` above prefers an sRGB surface — so
        // passing it is correct and no colour correction of our own is wanted.
        let egui = egui_wgpu::Renderer::new(
            &device,
            format,
            egui_wgpu::RendererOptions {
                // No MSAA: egui feathers its own edges, and there is no 3D embedded in this UI.
                msaa_samples: 1,
                // The video pass has no depth or stencil attachment, and egui needs neither.
                depth_stencil_format: None,
                ..Default::default()
            },
        );

        Ok(Gpu {
            window,
            surface,
            device,
            queue,
            config,
            pipeline,
            bind_layout,
            sampler,
            texture: None,
            egui,
        })
    }

    /// The present mode the surface was configured with, for the window title and the logs.
    pub fn present_mode(&self) -> &'static str {
        mode_name(self.config.present_mode)
    }

    /// The dimensions of the frame currently uploaded, or `None` before the first one.
    fn frame_dims(&self) -> Option<(u32, u32)> {
        self.texture.as_ref().map(|t| (t.width, t.height))
    }

    fn reconfigure(&mut self, size: Option<(u32, u32)>) {
        if let Some((w, h)) = size {
            self.config.width = w.max(1);
            self.config.height = h.max(1);
        }
        self.surface.configure(&self.device, &self.config);
    }

    /// Upload a decoded frame, replacing the texture when JPEG dimensions change.
    fn upload(&mut self, frame: &DecodedFrame) {
        if frame.width == 0 || frame.height == 0 {
            return;
        }
        let expected = DecodedFrame::expected_len(frame.width, frame.height);
        if frame.rgba.len() < expected {
            log::warn!(
                "render: frame {}x{} carries {} bytes, expected {expected}; skipped",
                frame.width,
                frame.height,
                frame.rgba.len()
            );
            return;
        }
        let stale = match &self.texture {
            Some(t) => t.width != frame.width || t.height != frame.height,
            None => true,
        };
        if stale {
            log::info!("render: video is now {}x{}", frame.width, frame.height);
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("nanokvm-video-texture"),
                size: wgpu::Extent3d {
                    width: frame.width,
                    height: frame.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                // sRGB: the decoder emits sRGB-encoded RGBA, and the surface format is sRGB too,
                // so sampling decodes and the target re-encodes. Plain Unorm would double the
                // gamma and wash the image out.
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("nanokvm-video-bind-group"),
                layout: &self.bind_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
            self.texture = Some(VideoTexture {
                texture,
                width: frame.width,
                height: frame.height,
                bind_group,
            });
        }
        let Some(t) = self.texture.as_ref() else {
            return;
        };
        self.queue.write_texture(
            t.texture.as_image_copy(),
            &frame.rgba[..expected],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(frame.width * 4),
                rows_per_image: Some(frame.height),
            },
            wgpu::Extent3d {
                width: frame.width,
                height: frame.height,
                depth_or_array_layers: 1,
            },
        );
    }

    /// Draw one frame: clear, then draw the video quad into its letterboxed viewport.
    ///
    /// Returns the video rectangle that was drawn, or `None` if there is no image yet. The
    /// caller times `present()` around this — the block is the point.
    fn draw(
        &mut self,
        shared: &RenderShared,
        captured_at: Option<Duration>,
        chrome: Option<&mut ChromeFrame>,
    ) -> DrawOutcome {
        let acquired = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(wgpu::SurfaceError::Timeout) => return DrawOutcome::Skipped,
            Err(e @ (wgpu::SurfaceError::Outdated | wgpu::SurfaceError::Lost)) => {
                log::debug!("render: surface {e:?}; reconfiguring");
                self.reconfigure(None);
                lock(&shared.stats).surface_recoveries += 1;
                return DrawOutcome::Skipped;
            }
            Err(e @ wgpu::SurfaceError::OutOfMemory) => {
                return DrawOutcome::Fatal(format!(
                    "the GPU ran out of memory acquiring a frame ({e}); the renderer cannot \
                     continue"
                ));
            }
            Err(e) => {
                return DrawOutcome::Fatal(format!("acquiring a surface frame failed: {e}"));
            }
        };

        let view = acquired
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("nanokvm-render"),
            });
        let rect = self
            .texture
            .as_ref()
            .and_then(|t| letterbox((self.config.width, self.config.height), (t.width, t.height)));

        // The chrome's own timing, measured around its three calls and nothing else.
        let mut egui_us = 0u64;
        let mut egui_cbs: Vec<wgpu::CommandBuffer> = Vec::new();
        let mut egui_free: Vec<egui::TextureId> = Vec::new();
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.config.width, self.config.height],
            pixels_per_point: chrome.as_ref().map_or(1.0, |c| c.pixels_per_point),
        };
        // Upload the atlas deltas and the vertex buffers before the pass begins: both encode
        // copies, and a pass in progress forbids that.
        //
        // A frame laid out for a different surface size is skipped, not scaled: between a
        // resize and the next chrome build the retained frame belongs to the old surface, and
        // drawing it would put the pill at the wrong place — over the video, on a window the user
        // is still dragging. Skipping costs at most one frame of a stale pill not appearing: the
        // event loop marks the chrome dirty on `Resized` and rebuilds at the new size. The
        // frame's texture deltas are left untouched, so nothing is lost from the
        // atlas (`chrome::frame`'s module docs).
        let surface = (self.config.width, self.config.height);
        let chrome = match chrome {
            Some(c) if !chrome_fits_surface(c.size_in_pixels, surface) => {
                log::debug!(
                    "render: chrome frame is {:?} but the surface is {surface:?}; skipping it \
                     this pass",
                    c.size_in_pixels
                );
                None
            }
            Some(c) if !c.primitives.is_empty() => {
                let t0 = Instant::now();
                for (id, delta) in std::mem::take(&mut c.textures_delta.set) {
                    self.egui
                        .update_texture(&self.device, &self.queue, id, &delta);
                }
                // Freed only *after* painting, per `FullOutput::textures_delta`'s own contract:
                // the frame being drawn may still reference a texture this delta retires.
                egui_free = std::mem::take(&mut c.textures_delta.free);
                egui_cbs = self.egui.update_buffers(
                    &self.device,
                    &self.queue,
                    &mut encoder,
                    &c.primitives,
                    &screen,
                );
                egui_us += t0.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
                Some(&*c)
            }
            _ => None,
        };

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("nanokvm-video-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(EMPTY_CLEAR),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            if let (Some(t), Some(r)) = (self.texture.as_ref(), rect) {
                // A viewport of zero extent is invalid; a window narrower than one video pixel is
                // possible while a resize is in flight.
                if r.width >= 1.0 && r.height >= 1.0 {
                    pass.set_viewport(r.x, r.y, r.width, r.height, 0.0, 1.0);
                    pass.set_pipeline(&self.pipeline);
                    pass.set_bind_group(0, &t.bind_group, &[]);
                    pass.draw(0..3, 0..1);
                }
            }
            // The chrome draws after the video, in this pass, and the order is required.
            // The video quad sets a letterboxed viewport; `Renderer::render` sets its own viewport
            // and resets only the *scissor* when it is done, never the viewport. Drawing egui
            // first — or reusing this pass for the video afterwards — would letterbox the chrome
            // into the video rectangle. `forget_lifetime` is what `Renderer::render`'s
            // `&mut RenderPass<'static>` requires; the pass is dropped at the end of this block
            // either way.
            if let Some(c) = chrome {
                let t0 = Instant::now();
                let mut pass = pass.forget_lifetime();
                self.egui.render(&mut pass, &c.primitives, &screen);
                egui_us += t0.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
            }
        }

        // egui's staging buffers must be submitted ahead of the encoder that reads them.
        self.queue.submit(
            egui_cbs
                .into_iter()
                .chain(std::iter::once(encoder.finish())),
        );
        for id in egui_free {
            self.egui.free_texture(&id);
        }
        if egui_us > 0 {
            let mut st = lock(&shared.stats);
            StatsInner::push(&mut st.egui_us, egui_us);
        }
        // Measure age at submission; this does not establish screen presentation time.
        if let Some(t) = captured_at {
            let age = now_monotonic().saturating_sub(t);
            let mut s = lock(&shared.stats);
            StatsInner::push(
                &mut s.age_us,
                age.as_micros().min(u128::from(u64::MAX)) as u64,
            );
        }

        self.window.pre_present_notify();
        let t0 = Instant::now();
        acquired.present();
        let present_us = t0.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        {
            let mut s = lock(&shared.stats);
            s.presented += 1;
            StatsInner::push(&mut s.present_us, present_us);
        }

        DrawOutcome::Presented
    }
}

enum DrawOutcome {
    Presented,
    /// Nothing was presented this pass; try again.
    Skipped,
    Fatal(String),
}

/// Start the `nanokvm-render` thread.
///
/// The thread owns `gpu` for the rest of its life. It ends when [`RenderShared::stop`] is called
/// or when it hits an unrecoverable surface error, which it records in
/// [`RenderShared::fatal`] for the event loop to report.
pub fn spawn(
    gpu: Gpu,
    frames: Arc<Slot<DecodedFrame>>,
    shared: Arc<RenderShared>,
    chrome: ChromeReceiver,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("nanokvm-render".to_string())
        .spawn(move || render_loop(gpu, &frames, &shared, &chrome))
}

/// What one pass of the render loop decided.
enum Step {
    /// Keep going.
    Continue,
    /// Stop, and tell the event loop why.
    Fatal(String),
}

/// The mutable state one pass carries into the next.
struct Passes {
    captured_at: Option<Duration>,
    /// Whether there is anything new to draw. Starts `true` so the first pass paints the empty
    /// clear colour rather than leaving a blank surface.
    pending: bool,
    /// The newest chrome, retained across passes so a redraw caused by a video frame still
    /// carries the pill. Its `textures_delta` is drained by the draw that applies it; its
    /// `primitives` stay until the event loop sends a new frame.
    chrome: Option<ChromeFrame>,
}

/// Raises [`RenderShared`]'s finished flag however the thread leaves — returning, breaking, or
/// unwinding past this frame — so a bounded join can tell "gone" from "still inside `present()`".
struct FinishGuard<'a>(&'a RenderShared);

impl Drop for FinishGuard<'_> {
    fn drop(&mut self) {
        self.0.mark_finished();
    }
}

/// Render pending work and report fatal failures to the event loop.
///
/// Fresh frames replace stale work. Without new frames, redraw only for resize or
/// a deferred present. Panics become visible failure instead of a silently dead worker.
fn render_loop(
    mut gpu: Gpu,
    frames: &Arc<Slot<DecodedFrame>>,
    shared: &Arc<RenderShared>,
    chrome: &ChromeReceiver,
) {
    let _finished = FinishGuard(shared);
    let mut passes = Passes {
        captured_at: None,
        pending: true,
        chrome: None,
    };
    while !shared.stop.load(Ordering::SeqCst) {
        let step = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            render_pass(&mut gpu, frames, shared, chrome, &mut passes)
        }));
        let outcome = match step {
            Ok(step) => step,
            Err(payload) => Step::Fatal(format!(
                "the render thread panicked: {}",
                panic_message(payload.as_ref())
            )),
        };
        if let Step::Fatal(msg) = outcome {
            log::error!("render: {msg}");
            *lock(&shared.fatal) = Some(msg);
            break;
        }
    }
    // GPU destruction may call into Wayland after the connection has closed.
    // Only the event loop knows whether that connection is still alive.
    release_gpu(gpu);
    log::info!("render thread finished");
}

/// Retain GPU objects until process exit instead of running their destructors.
///
/// The bounded render join can expire during EGL teardown, allowing winit to close
/// the Wayland connection underneath it. Checking a flag before teardown cannot
/// prevent that race. The render loop only exits during process shutdown; revisit
/// this lifetime policy if rendering can stop while the process continues.
fn release_gpu<T>(gpu: T) {
    log::debug!("render: leaving the GPU objects to the process exit");
    std::mem::forget(gpu);
}

/// One pass of the render loop. See [`render_loop`].
fn render_pass(
    gpu: &mut Gpu,
    frames: &Slot<DecodedFrame>,
    shared: &RenderShared,
    chrome: &ChromeReceiver,
    passes: &mut Passes,
) -> Step {
    let frame = frames.wait_take(FRAME_WAIT);
    // UI updates must trigger rendering even when capture has stopped. Otherwise
    // hover tooltips would wait indefinitely for another video frame.
    if let Some(new_chrome) = chrome.drain() {
        match passes.chrome.as_mut() {
            // Absorb rather than replace: the retained frame may still be holding a
            // `textures_delta` that no draw has applied yet (an occluded surface, say), and
            // dropping it would lose a font-atlas page permanently.
            Some(existing) => existing.absorb(new_chrome),
            None => passes.chrome = Some(new_chrome),
        }
        passes.pending = true;
    }
    let closed = frame.is_none() && frames.is_closed();
    if let Some(f) = frame {
        gpu.upload(&f);
        // Published from here rather than from `draw`, so the event loop can map a cursor while
        // the surface is occluded and nothing is being presented at all.
        if let Some(dims) = gpu.frame_dims() {
            *lock(&shared.frame) = Some(dims);
        }
        passes.captured_at = Some(f.captured_at);
        passes.pending = true;
    }
    if let Some(size) = lock(&shared.resize).take() {
        gpu.reconfigure(Some(size));
        passes.pending = true;
    }
    // Skip presentation while hidden to avoid waiting for withheld compositor callbacks.
    // Keep consuming frames so the first visible redraw uses the current image.
    if shared.is_occluded() || !passes.pending {
        if closed {
            // Nothing will ever arrive from a closed slot, so `wait_take` returns instantly: sleep
            // rather than spin. The last image stays on screen.
            std::thread::sleep(FRAME_WAIT);
        }
        return Step::Continue;
    }
    match gpu.draw(shared, passes.captured_at, passes.chrome.as_mut()) {
        DrawOutcome::Presented => passes.pending = false,
        DrawOutcome::Skipped => {
            // Same reasoning as above: a skipped present leaves `pending` set, and with a closed
            // slot the next `wait_take` returns immediately, so this arm would spin.
            if closed {
                std::thread::sleep(FRAME_WAIT);
            }
        }
        DrawOutcome::Fatal(msg) => return Step::Fatal(msg),
    }
    Step::Continue
}

/// Whether UI geometry matches the current surface size. Reject stale layout after
/// a resize while retaining its pending texture updates.
fn chrome_fits_surface(frame: [u32; 2], surface: (u32, u32)) -> bool {
    frame == [surface.0, surface.1]
}

/// Best-effort text of a panic payload, for [`RenderShared::fatal`].
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "a panic payload of an unknown type".to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// Counts its own destruction, which is the only thing [`release_gpu`] decides.
    struct CountsItsDrops(Arc<AtomicUsize>);

    impl Drop for CountsItsDrops {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// GPU destructors must not run after bounded shutdown can close the Wayland display.
    #[test]
    fn the_render_threads_gpu_objects_are_never_destructed() {
        let drops = Arc::new(AtomicUsize::new(0));
        release_gpu(CountsItsDrops(Arc::clone(&drops)));
        assert_eq!(
            drops.load(Ordering::SeqCst),
            0,
            "a destructor ran, and it would have reached libwayland"
        );
    }

    #[test]
    fn letterbox_pillarboxes_a_wide_window() {
        // 16:9 video in a 32:9 window: full height, half width, centred horizontally.
        let r = letterbox((3840, 1080), (1920, 1080)).expect("rect");
        assert_eq!(r.height, 1080.0);
        assert_eq!(r.width, 1920.0);
        assert_eq!(r.x, 960.0);
        assert_eq!(r.y, 0.0);
    }

    #[test]
    fn letterbox_bars_a_tall_window() {
        // 16:9 video in a 16:18 window: full width, half height, centred vertically.
        let r = letterbox((1920, 2160), (1920, 1080)).expect("rect");
        assert_eq!(r.width, 1920.0);
        assert_eq!(r.height, 1080.0);
        assert_eq!(r.x, 0.0);
        assert_eq!(r.y, 540.0);
    }

    #[test]
    fn letterbox_exact_fit_has_no_bars() {
        let r = letterbox((1920, 1080), (1920, 1080)).expect("rect");
        assert_eq!(
            r,
            Rect {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0
            }
        );
    }

    #[test]
    fn letterbox_scales_down_to_a_small_window() {
        let r = letterbox((960, 1000), (1920, 1080)).expect("rect");
        assert_eq!(r.width, 960.0);
        assert_eq!(r.height, 540.0);
        assert_eq!(r.x, 0.0);
        assert_eq!(r.y, 230.0);
    }

    #[test]
    fn letterbox_rejects_degenerate_rectangles() {
        assert_eq!(letterbox((0, 1080), (1920, 1080)), None);
        assert_eq!(letterbox((1920, 0), (1920, 1080)), None);
        assert_eq!(letterbox((1920, 1080), (0, 1080)), None);
        assert_eq!(letterbox((1920, 1080), (1920, 0)), None);
    }

    #[test]
    fn present_mode_prefers_fifo_from_what_is_offered() {
        // The list niri offers.
        assert_eq!(
            choose_present_mode(&[wgpu::PresentMode::Mailbox, wgpu::PresentMode::Fifo]),
            Some(wgpu::PresentMode::Fifo)
        );
        // Fifo absent: take the first offered rather than naming one that is not there.
        assert_eq!(
            choose_present_mode(&[wgpu::PresentMode::Mailbox]),
            Some(wgpu::PresentMode::Mailbox)
        );
        assert_eq!(choose_present_mode(&[]), None);
    }

    #[test]
    fn percentiles_are_by_nearest_rank() {
        let mut v = vec![5, 1, 4, 2, 3];
        assert_eq!(percentile(&mut v, 0.5), 3);
        assert_eq!(percentile(&mut v, 1.0), 5);
        assert_eq!(percentile(&mut [], 0.5), 0);
    }

    #[test]
    fn stats_drain_resets_counters() {
        let shared = RenderShared::new();
        {
            let mut s = lock(&shared.stats);
            s.presented = 3;
            StatsInner::push(&mut s.present_us, 6800);
            StatsInner::push(&mut s.present_us, 6900);
            StatsInner::push(&mut s.present_us, 7000);
            StatsInner::push(&mut s.age_us, 16_000);
        }
        let first = shared.take_stats();
        assert_eq!(first.presented, 3);
        assert_eq!(first.present_p50_us, 6900);
        assert_eq!(first.present_max_us, 7000);
        assert_eq!(first.age_p50_us, 16_000);
        let second = shared.take_stats();
        assert_eq!(second, RenderStats::default());
    }

    /// Discard UI geometry laid out for another surface size instead of
    /// rendering stretched controls after resize.
    #[test]
    fn a_chrome_frame_is_only_drawn_on_the_surface_it_was_laid_out_for() {
        assert!(chrome_fits_surface([1920, 1080], (1920, 1080)));
        // The frame or two between a resize and the next chrome build.
        assert!(!chrome_fits_surface([1920, 1080], (1280, 720)));
        assert!(!chrome_fits_surface([1920, 1080], (1920, 1081)));
        assert!(
            !chrome_fits_surface([1080, 1920], (1920, 1080)),
            "not just the area"
        );
    }

    #[test]
    fn sample_ring_is_bounded() {
        let mut ring = VecDeque::new();
        for i in 0..(SAMPLE_CAPACITY as u64 + 100) {
            StatsInner::push(&mut ring, i);
        }
        assert_eq!(ring.len(), SAMPLE_CAPACITY);
        assert_eq!(*ring.back().expect("last"), SAMPLE_CAPACITY as u64 + 99);
    }
}
