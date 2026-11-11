use std::{
    ffi::{CStr, c_void},
    num::NonZeroU32,
};

use anyhow::{Context as _, Result, anyhow, bail};
use glow::HasContext;
use glutin::{
    config::{Config, ConfigTemplateBuilder},
    context::{ContextApi, ContextAttributesBuilder, PossiblyCurrentContext, Version},
    display::{AsRawDisplay, Display, GetGlDisplay, GlDisplay, RawDisplay},
    prelude::{GlSurface, NotCurrentGlContext, PossiblyCurrentGlContext},
    surface::{Surface, SwapInterval, WindowSurface},
};
use glutin_winit::{ApiPreference, DisplayBuilder, GlWindow};
use raw_window_handle::HasWindowHandle;
use winit::{
    event_loop::ActiveEventLoop,
    window::{Window, WindowAttributes},
};

use crate::{
    Decoded, Picture,
    decode::{ColorInfo, Plane},
};

const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: i32 = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: i32 = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: i32 = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: i32 = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: i32 = 0x3444;
const EGL_NONE: i32 = 0x3038;
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

type EglImage = *const c_void;
type CreateImage =
    unsafe extern "C" fn(*const c_void, *const c_void, u32, *const c_void, *const i32) -> EglImage;
type DestroyImage = unsafe extern "C" fn(*const c_void, EglImage) -> u32;
type ImageTargetTexture = unsafe extern "C" fn(u32, EglImage);

const VERTEX: &str = r#"#version 300 es
uniform vec4 u_rect;  // left, top, right, bottom in clip space
uniform vec2 u_texmax;   // texture coordinates at the bottom-right corner
out vec2 v_tex;
void main() {
    vec2 c = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1));
    gl_Position = vec4(mix(u_rect.x, u_rect.z, c.x), mix(u_rect.y, u_rect.w, c.y), 0.0, 1.0);
    v_tex = c * u_texmax;
}
"#;

const NV12: &str = r#"#version 300 es
precision highp float;
uniform sampler2D u_y;
uniform sampler2D u_uv;
uniform mat3 u_matrix;
uniform vec3 u_offset;
in vec2 v_tex;
out vec4 o;
void main() {
    vec3 yuv = vec3(texture(u_y, v_tex).r, texture(u_uv, v_tex).rg) - u_offset;
    o = vec4(clamp(u_matrix * yuv, 0.0, 1.0), 1.0);
}
"#;

const BGRX: &str = r#"#version 300 es
precision mediump float;
uniform sampler2D u_tex;
in vec2 v_tex;
out vec4 o;
void main() { o = vec4(texture(u_tex, v_tex).bgr, 1.0); }
"#;

const RGBA: &str = r#"#version 300 es
precision mediump float;
uniform sampler2D u_tex;
in vec2 v_tex;
out vec4 o;
void main() { o = texture(u_tex, v_tex); }
"#;

fn yuv_to_rgb(c: ColorInfo) -> ([f32; 9], [f32; 3]) {
    let (a, b, d, e) = if c.bt709 {
        (1.5748, 0.187324, 0.468124, 1.8556)
    } else {
        (1.402, 0.344136, 0.714136, 1.772)
    };
    let (ys, cs, y0) = if c.full_range {
        (1.0, 1.0, 0.0)
    } else {
        (255.0 / 219.0, 255.0 / 224.0, 16.0 / 255.0)
    };
    let m = [ys, ys, ys, 0.0, -b * cs, e * cs, a * cs, -d * cs, 0.0];
    (m, [y0, 128.0 / 255.0, 128.0 / 255.0])
}

struct Programs {
    nv12: glow::Program,
    bgrx: glow::Program,
    rgba: glow::Program,
    vao: glow::VertexArray,
}

pub struct Renderer {
    display: Display,
    config: Config,
    context: PossiblyCurrentContext,
    gl: glow::Context,
    egl_display: *const c_void,
    create_image: CreateImage,
    destroy_image: DestroyImage,
    image_target_texture: ImageTargetTexture,
    programs: Programs,
    swap_interval: SwapInterval,
}

pub struct WindowGl {
    surface: Surface<WindowSurface>,
    size: (u32, u32),
    y_tex: glow::Texture,
    uv_tex: glow::Texture,
    rgb_tex: glow::Texture,
    overlay_tex: glow::Texture,
    images: Vec<EglImage>,
    loaded: Option<u64>,
    overlay: Option<(String, u32, u32)>,
}

impl Renderer {
    pub fn new(
        event_loop: &ActiveEventLoop,
        attrs: WindowAttributes,
    ) -> Result<(Renderer, Window, WindowGl)> {
        let (window, config) = DisplayBuilder::new()
            .with_preference(ApiPreference::PreferEgl)
            .with_window_attributes(Some(attrs))
            .build(event_loop, ConfigTemplateBuilder::new(), |mut configs| {
                configs.next().expect("a GL config")
            })
            .map_err(|e| anyhow!("EGL setup: {e}"))?;
        let window = window.context("no window from glutin")?;
        let display = config.display();
        let egl_display = match display.raw_display() {
            RawDisplay::Egl(d) => d,
            #[allow(unreachable_patterns)]
            _ => bail!("not an EGL display (dma-buf import needs EGL)"),
        };
        let raw_window = window
            .window_handle()
            .map_err(|e| anyhow!("window handle: {e}"))?
            .as_raw();
        let attributes = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::Gles(Some(Version::new(3, 0))))
            .build(Some(raw_window));
        let not_current =
            unsafe { display.create_context(&config, &attributes) }.context("GL context")?;
        let surface = create_surface(&display, &config, &window)?;
        let context = not_current.make_current(&surface).context("make current")?;
        let gl =
            unsafe { glow::Context::from_loader_function_cstr(|s| display.get_proc_address(s)) };

        let proc = |name: &CStr| {
            let p = display.get_proc_address(name);
            if p.is_null() {
                Err(anyhow!("missing {name:?}"))
            } else {
                Ok(p)
            }
        };
        let create_image: CreateImage = unsafe { std::mem::transmute(proc(c"eglCreateImageKHR")?) };
        let destroy_image: DestroyImage =
            unsafe { std::mem::transmute(proc(c"eglDestroyImageKHR")?) };
        let image_target_texture: ImageTargetTexture =
            unsafe { std::mem::transmute(proc(c"glEGLImageTargetTexture2DOES")?) };

        let programs = unsafe { compile_programs(&gl)? };
        let swap_interval = if std::env::var("WINSTREAM_VSYNC").is_ok_and(|v| v == "1") {
            SwapInterval::Wait(NonZeroU32::MIN)
        } else {
            SwapInterval::DontWait
        };
        let mut renderer = Renderer {
            display,
            config,
            context,
            gl,
            egl_display,
            create_image,
            destroy_image,
            image_target_texture,
            programs,
            swap_interval,
        };
        let wgl = renderer.window_gl(surface, &window)?;
        Ok((renderer, window, wgl))
    }

    pub fn new_window(
        &mut self,
        event_loop: &ActiveEventLoop,
        attrs: WindowAttributes,
    ) -> Result<(Window, WindowGl)> {
        let window = glutin_winit::finalize_window(event_loop, attrs, &self.config)
            .context("create window")?;
        let surface = create_surface(&self.display, &self.config, &window)?;
        let wgl = self.window_gl(surface, &window)?;
        Ok((window, wgl))
    }

    fn window_gl(&mut self, surface: Surface<WindowSurface>, window: &Window) -> Result<WindowGl> {
        self.context
            .make_current(&surface)
            .context("make current")?;
        let _ = surface.set_swap_interval(&self.context, self.swap_interval);
        let gl = &self.gl;
        let texture = |filter: i32| -> Result<glow::Texture> {
            unsafe {
                let t = gl.create_texture().map_err(|e| anyhow!("texture: {e}"))?;
                gl.bind_texture(glow::TEXTURE_2D, Some(t));
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, filter);
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, filter);
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_WRAP_S,
                    glow::CLAMP_TO_EDGE as i32,
                );
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_WRAP_T,
                    glow::CLAMP_TO_EDGE as i32,
                );
                Ok(t)
            }
        };
        let size = window.inner_size();
        Ok(WindowGl {
            surface,
            size: (size.width, size.height),
            y_tex: texture(glow::NEAREST as i32)?,
            uv_tex: texture(glow::LINEAR as i32)?,
            rgb_tex: texture(glow::NEAREST as i32)?,
            overlay_tex: texture(glow::NEAREST as i32)?,
            images: Vec::new(),
            loaded: None,
            overlay: None,
        })
    }

    pub fn close_window(&mut self, mut wgl: WindowGl) {
        let _ = self.context.make_current(&wgl.surface);
        self.release_images(&mut wgl);
        unsafe {
            for t in [wgl.y_tex, wgl.uv_tex, wgl.rgb_tex, wgl.overlay_tex] {
                self.gl.delete_texture(t);
            }
        }
    }

    fn release_images(&self, wgl: &mut WindowGl) {
        for image in wgl.images.drain(..) {
            unsafe { (self.destroy_image)(self.egl_display, image) };
        }
    }

    pub fn draw(
        &mut self,
        wgl: &mut WindowGl,
        window: &Window,
        frame: &Decoded,
        overlay: Option<&str>,
        capture: bool,
    ) -> Result<Option<Vec<u8>>> {
        self.context
            .make_current(&wgl.surface)
            .context("make current")?;
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return Ok(None);
        };
        if wgl.size != (size.width, size.height) {
            wgl.surface.resize(&self.context, w, h);
            wgl.size = (size.width, size.height);
        }
        let (ww, wh) = (size.width as f32, size.height as f32);
        let gl = &self.gl;
        let p = &self.programs;
        unsafe {
            gl.viewport(0, 0, size.width as i32, size.height as i32);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            gl.bind_vertex_array(Some(p.vao));
        }
        let rect = [
            -1.0,
            1.0,
            -1.0 + 2.0 * frame.width as f32 / ww,
            1.0 - 2.0 * frame.height as f32 / wh,
        ];
        let id = frame.header.frame_id;
        match &frame.picture {
            Picture::Gpu(gpu) => {
                if wgl.loaded != Some(id) {
                    self.release_images(wgl);
                    for (plane, tex) in gpu.planes.iter().zip([wgl.y_tex, wgl.uv_tex]) {
                        let image = self.import(plane)?;
                        wgl.images.push(image);
                        unsafe {
                            self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                            (self.image_target_texture)(glow::TEXTURE_2D, image);
                        }
                    }
                    wgl.loaded = Some(id);
                }
                let (matrix, offset) = yuv_to_rgb(gpu.color);
                let gl = &self.gl;
                let y = &gpu.planes[0];
                unsafe {
                    gl.use_program(Some(p.nv12));
                    set_rect(
                        gl,
                        p.nv12,
                        rect,
                        [
                            frame.width as f32 / y.width as f32,
                            frame.height as f32 / y.height as f32,
                        ],
                    );
                    gl.active_texture(glow::TEXTURE0);
                    gl.bind_texture(glow::TEXTURE_2D, Some(wgl.y_tex));
                    gl.active_texture(glow::TEXTURE1);
                    gl.bind_texture(glow::TEXTURE_2D, Some(wgl.uv_tex));
                    gl.uniform_1_i32(gl.get_uniform_location(p.nv12, "u_y").as_ref(), 0);
                    gl.uniform_1_i32(gl.get_uniform_location(p.nv12, "u_uv").as_ref(), 1);
                    gl.uniform_matrix_3_f32_slice(
                        gl.get_uniform_location(p.nv12, "u_matrix").as_ref(),
                        false,
                        &matrix,
                    );
                    gl.uniform_3_f32_slice(
                        gl.get_uniform_location(p.nv12, "u_offset").as_ref(),
                        &offset,
                    );
                    gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                    gl.active_texture(glow::TEXTURE0);
                }
            }
            Picture::Cpu(pixels) => {
                let gl = &self.gl;
                unsafe {
                    gl.active_texture(glow::TEXTURE0);
                    gl.bind_texture(glow::TEXTURE_2D, Some(wgl.rgb_tex));
                    if wgl.loaded != Some(id) {
                        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
                        gl.tex_image_2d(
                            glow::TEXTURE_2D,
                            0,
                            glow::RGBA8 as i32,
                            frame.width as i32,
                            frame.height as i32,
                            0,
                            glow::RGBA,
                            glow::UNSIGNED_BYTE,
                            glow::PixelUnpackData::Slice(Some(bytemuck::cast_slice(pixels))),
                        );
                        wgl.loaded = Some(id);
                    }
                    gl.use_program(Some(p.bgrx));
                    set_rect(gl, p.bgrx, rect, [1.0, 1.0]);
                    gl.uniform_1_i32(gl.get_uniform_location(p.bgrx, "u_tex").as_ref(), 0);
                    gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                }
            }
        }
        let shot =
            capture.then(|| unsafe { read_rgb(&self.gl, frame.width, frame.height, size.height) });
        if let Some(text) = overlay {
            self.draw_overlay(wgl, text, ww, wh);
        }
        wgl.surface
            .swap_buffers(&self.context)
            .context("swap buffers")?;
        Ok(shot)
    }

    fn import(&self, plane: &Plane) -> Result<EglImage> {
        let mut attrs = vec![
            EGL_WIDTH,
            plane.width as i32,
            EGL_HEIGHT,
            plane.height as i32,
            EGL_LINUX_DRM_FOURCC_EXT,
            plane.fourcc as i32,
            EGL_DMA_BUF_PLANE0_FD_EXT,
            plane.fd,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            plane.offset as i32,
            EGL_DMA_BUF_PLANE0_PITCH_EXT,
            plane.pitch as i32,
        ];
        if plane.modifier != DRM_FORMAT_MOD_INVALID {
            attrs.extend([
                EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
                plane.modifier as u32 as i32,
                EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
                (plane.modifier >> 32) as u32 as i32,
            ]);
        }
        attrs.push(EGL_NONE);
        let image = unsafe {
            (self.create_image)(
                self.egl_display,
                std::ptr::null(),
                EGL_LINUX_DMA_BUF_EXT,
                std::ptr::null(),
                attrs.as_ptr(),
            )
        };
        if image.is_null() {
            bail!(
                "eglCreateImageKHR failed for a {}x{} plane (fourcc {:#x}, modifier {:#x})",
                plane.width,
                plane.height,
                plane.fourcc,
                plane.modifier
            );
        }
        Ok(image)
    }

    fn draw_overlay(&self, wgl: &mut WindowGl, text: &str, ww: f32, wh: f32) {
        let gl = &self.gl;
        let p = &self.programs;
        unsafe {
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(wgl.overlay_tex));
            if wgl.overlay.as_ref().is_none_or(|(t, _, _)| t != text) {
                let (w, h, rgba) = overlay_rgba(text);
                gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA8 as i32,
                    w as i32,
                    h as i32,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(Some(&rgba)),
                );
                wgl.overlay = Some((text.to_string(), w, h));
            }
            let (_, w, h) = wgl.overlay.as_ref().unwrap();
            let (x0, y0) = (8.0, 8.0);
            let rect = [
                -1.0 + 2.0 * x0 / ww,
                1.0 - 2.0 * y0 / wh,
                -1.0 + 2.0 * (x0 + *w as f32) / ww,
                1.0 - 2.0 * (y0 + *h as f32) / wh,
            ];
            gl.enable(glow::BLEND);
            gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
            gl.use_program(Some(p.rgba));
            set_rect(gl, p.rgba, rect, [1.0, 1.0]);
            gl.uniform_1_i32(gl.get_uniform_location(p.rgba, "u_tex").as_ref(), 0);
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            gl.disable(glow::BLEND);
        }
    }
}

fn create_surface(
    display: &Display,
    config: &Config,
    window: &Window,
) -> Result<Surface<WindowSurface>> {
    let attrs = window
        .build_surface_attributes(Default::default())
        .map_err(|e| anyhow!("surface attributes: {e}"))?;
    unsafe { display.create_window_surface(config, &attrs) }.context("window surface")
}

unsafe fn set_rect(gl: &glow::Context, program: glow::Program, rect: [f32; 4], tex: [f32; 2]) {
    unsafe {
        gl.uniform_4_f32_slice(gl.get_uniform_location(program, "u_rect").as_ref(), &rect);
        gl.uniform_2_f32_slice(gl.get_uniform_location(program, "u_texmax").as_ref(), &tex);
    }
}

unsafe fn read_rgb(gl: &glow::Context, width: u32, height: u32, window_height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut rgba = vec![0u8; w * h * 4];
    unsafe {
        gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
        gl.read_pixels(
            0,
            window_height as i32 - height as i32,
            width as i32,
            height as i32,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelPackData::Slice(Some(&mut rgba)),
        );
    }
    let mut rgb = Vec::with_capacity(w * h * 3);
    for row in (0..h).rev() {
        for px in rgba[row * w * 4..(row + 1) * w * 4].as_chunks::<4>().0 {
            rgb.extend_from_slice(&px[..3]);
        }
    }
    rgb
}

fn overlay_rgba(text: &str) -> (u32, u32, Vec<u8>) {
    use font8x8::UnicodeFonts;
    const SCALE: usize = 2;
    const PAD: usize = 6;
    let glyph = 8 * SCALE;
    let (w, h) = (text.chars().count() * glyph + 2 * PAD, glyph + 2 * PAD);
    let mut rgba: Vec<u8> = [0, 0, 0, 191].repeat(w * h);
    for (i, c) in text.chars().enumerate() {
        let Some(rows) = font8x8::BASIC_FONTS
            .get(c)
            .or_else(|| font8x8::LATIN_FONTS.get(c))
        else {
            continue;
        };
        let cx = PAD + i * glyph;
        for (row, bits) in rows.iter().enumerate() {
            for col in 0..8 {
                if bits & (1 << col) == 0 {
                    continue;
                }
                for dy in 0..SCALE {
                    for dx in 0..SCALE {
                        let (x, y) = (cx + col * SCALE + dx, PAD + row * SCALE + dy);
                        rgba[(y * w + x) * 4..(y * w + x) * 4 + 4]
                            .copy_from_slice(&[255, 255, 255, 255]);
                    }
                }
            }
        }
    }
    (w as u32, h as u32, rgba)
}

unsafe fn compile_programs(gl: &glow::Context) -> Result<Programs> {
    unsafe {
        let program = |fragment: &str| -> Result<glow::Program> {
            let program = gl.create_program().map_err(|e| anyhow!("program: {e}"))?;
            let mut shaders = Vec::new();
            for (kind, src) in [
                (glow::VERTEX_SHADER, VERTEX),
                (glow::FRAGMENT_SHADER, fragment),
            ] {
                let shader = gl.create_shader(kind).map_err(|e| anyhow!("shader: {e}"))?;
                gl.shader_source(shader, src);
                gl.compile_shader(shader);
                if !gl.get_shader_compile_status(shader) {
                    bail!("shader: {}", gl.get_shader_info_log(shader));
                }
                gl.attach_shader(program, shader);
                shaders.push(shader);
            }
            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                bail!("link: {}", gl.get_program_info_log(program));
            }
            for shader in shaders {
                gl.detach_shader(program, shader);
                gl.delete_shader(shader);
            }
            Ok(program)
        };
        Ok(Programs {
            nv12: program(NV12)?,
            bgrx: program(BGRX)?,
            rgba: program(RGBA)?,
            vao: gl
                .create_vertex_array()
                .map_err(|e| anyhow!("vertex array: {e}"))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bt601_limited() {
        let (m, off) = yuv_to_rgb(ColorInfo {
            bt709: false,
            full_range: false,
        });
        let apply = |y: f32, u: f32, v: f32| {
            let (y, u, v) = (y / 255.0 - off[0], u / 255.0 - off[1], v / 255.0 - off[2]);
            [
                m[0] * y + m[3] * u + m[6] * v,
                m[1] * y + m[4] * u + m[7] * v,
                m[2] * y + m[5] * u + m[8] * v,
            ]
        };
        let close = |a: [f32; 3], b: [f32; 3]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 0.01);
        assert!(close(apply(16.0, 128.0, 128.0), [0.0, 0.0, 0.0]));
        assert!(close(apply(235.0, 128.0, 128.0), [1.0, 1.0, 1.0]));
        let red = apply(81.0, 90.0, 240.0);
        assert!(red[0] > 0.98 && red[1] < 0.02 && red[2] < 0.02, "{red:?}");
    }
}
