//! GPU video path for mpv's OpenGL render API.
//!
//! mpv renders each frame (hardware decoded with VideoToolbox, scaled, colour
//! converted and with subtitles) into an RGB texture on an offscreen CGL
//! context. Two small shader passes then write that frame as NV12 (BT.601,
//! full range) into an IOSurface-backed `CVPixelBuffer`, which GPUI's
//! `surface()` element samples directly from Metal. Pixels therefore never
//! travel through the CPU.
//!
//! Everything here must stay on the thread that created the [`GlContext`].

use anyhow::{Result, anyhow, bail};
use cgl::{
    CGLChoosePixelFormat, CGLContextObj, CGLCreateContext, CGLDestroyContext, CGLError, CGLErrorString,
    CGLPixelFormatAttribute, CGLPixelFormatObj, CGLReleasePixelFormat, CGLSetCurrentContext, CGLTexImageIOSurface2D,
    kCGLPFAAccelerated, kCGLPFAAllowOfflineRenderers, kCGLPFAOpenGLProfile,
};
use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_video::pixel_buffer::{CVPixelBuffer, CVPixelBufferKeys, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange};
use core_video::pixel_buffer_io_surface::CVPixelBufferGetIOSurface;
use core_video::pixel_buffer_pool::CVPixelBufferPool;
use gl::types::{GLchar, GLenum, GLint, GLuint};
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::Once;

/// `kCGLOGLPVersion_3_2_Core` (missing from the `cgl` crate).
const OPENGL_3_2_CORE: CGLPixelFormatAttribute = 0x3200;
/// `RTLD_DEFAULT` from macOS `<dlfcn.h>` (missing from the `libc` crate).
const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

const VERTEX_SHADER: &str = r"#version 150
void main() {
    // One triangle covering the whole viewport: (-1,-1), (3,-1), (-1,3).
    vec2 corner = vec2((gl_VertexID << 1) & 2, gl_VertexID & 2);
    gl_Position = vec4(corner * 2.0 - 1.0, 0.0, 1.0);
}
";

// The coefficients are the exact inverse of the BT.601 full-range matrix in
// GPUI's `surface_fragment` shader, so colours survive the round trip.
const LUMA_SHADER: &str = r"#version 150
uniform sampler2D rgb;
out vec4 color;
void main() {
    vec3 c = texelFetch(rgb, ivec2(gl_FragCoord.xy), 0).rgb;
    color = vec4(dot(c, vec3(0.299, 0.587, 0.114)), 0.0, 0.0, 1.0);
}
";

// Each chroma texel covers 2x2 RGB texels. Sampling bilinearly at their
// shared corner averages all four in a single fetch.
const CHROMA_SHADER: &str = r"#version 150
uniform sampler2D rgb;
out vec4 color;
void main() {
    vec3 c = texture(rgb, gl_FragCoord.xy * 2.0 / vec2(textureSize(rgb, 0))).rgb;
    float cb = dot(c, vec3(-0.168736, -0.331264, 0.5)) + 0.5;
    float cr = dot(c, vec3(0.5, -0.418688, -0.081312)) + 0.5;
    color = vec4(cb, cr, 0.0, 1.0);
}
";

#[expect(unsafe_code, reason = "reads CGL's static error strings")]
fn cgl_check(error: CGLError, what: &str) -> Result<()> {
    if error == 0 {
        return Ok(());
    }
    // SAFETY: CGLErrorString accepts any error code and returns a static NUL-terminated string.
    let message = unsafe { CStr::from_ptr(CGLErrorString(error)) }.to_string_lossy();
    Err(anyhow!("{what} failed: {message} ({error})"))
}

#[expect(unsafe_code, reason = "looks up OpenGL entry points with dlsym")]
fn proc_address(name: &CStr) -> *mut c_void {
    // SAFETY: `name` is NUL-terminated, and RTLD_DEFAULT searches every loaded image
    // (OpenGL.framework is linked by the cgl crate).
    unsafe { libc::dlsym(RTLD_DEFAULT, name.as_ptr()) }
}

/// `get_proc_address` callback for `mpv_opengl_init_params`.
#[expect(unsafe_code, reason = "C callback for mpv's OpenGL loader")]
pub unsafe extern "C" fn mpv_get_proc_address(_context: *mut c_void, name: *const c_char) -> *mut c_void {
    // SAFETY: mpv passes a valid NUL-terminated function name.
    proc_address(unsafe { CStr::from_ptr(name) })
}

/// An OpenGL 3.2 core pixel format, released by the caller with `CGLReleasePixelFormat`.
#[expect(unsafe_code, reason = "calls CGLChoosePixelFormat")]
fn choose_pixel_format(accelerated: bool) -> Result<CGLPixelFormatObj> {
    let mut attributes = vec![kCGLPFAOpenGLProfile, OPENGL_3_2_CORE];
    if accelerated {
        attributes.push(kCGLPFAAccelerated);
    }
    attributes.extend([kCGLPFAAllowOfflineRenderers, 0]);
    let mut pixel_format = std::ptr::null_mut();
    let mut count: GLint = 0;
    // SAFETY: `attributes` is 0-terminated and both out-pointers are valid.
    cgl_check(
        unsafe { CGLChoosePixelFormat(attributes.as_ptr(), &mut pixel_format, &mut count) },
        "CGLChoosePixelFormat",
    )?;
    if pixel_format.is_null() {
        bail!("no OpenGL 3.2 pixel format (accelerated: {accelerated})");
    }
    Ok(pixel_format)
}

/// An offscreen OpenGL 3.2 core context, current on the creating thread.
pub struct GlContext(CGLContextObj);

impl GlContext {
    /// Create the context, make it current on this thread and load GL entry points.
    #[expect(unsafe_code, reason = "creates a CGL context")]
    pub fn new_current() -> Result<Self> {
        let pixel_format = match choose_pixel_format(true) {
            Ok(pixel_format) => pixel_format,
            // macOS virtual machines expose Metal but no accelerated OpenGL
            // renderer; Apple's software renderer still plays video there.
            Err(error) => {
                log::warn!("no accelerated OpenGL renderer ({error:#}), using the software renderer");
                choose_pixel_format(false)?
            }
        };
        let mut context = std::ptr::null_mut();
        // SAFETY: `pixel_format` is valid; the context keeps its own reference to it,
        // so ours is released right after.
        let created = unsafe { CGLCreateContext(pixel_format, std::ptr::null_mut(), &mut context) };
        // SAFETY: balances CGLChoosePixelFormat.
        unsafe { CGLReleasePixelFormat(pixel_format) };
        cgl_check(created, "CGLCreateContext")?;
        let this = Self(context);
        // SAFETY: `context` was just created and is not current on any other thread.
        cgl_check(unsafe { CGLSetCurrentContext(context) }, "CGLSetCurrentContext")?;
        // The gl crate keeps its entry points in global statics, so load them
        // once: reloading while an older render thread is still tearing down
        // would race. On macOS the addresses are the same for every context.
        static LOAD_GL: Once = Once::new();
        LOAD_GL.call_once(|| {
            gl::load_with(|name| match CString::new(name) {
                Ok(name) => proc_address(&name).cast_const(),
                Err(_) => std::ptr::null(),
            });
        });
        Ok(this)
    }
}

impl Drop for GlContext {
    #[expect(unsafe_code, reason = "destroys the CGL context")]
    fn drop(&mut self) {
        // SAFETY: the context was created in `new_current` and is destroyed only here.
        let results = unsafe { [CGLSetCurrentContext(std::ptr::null_mut()), CGLDestroyContext(self.0)] };
        for (result, what) in results.into_iter().zip(["CGLSetCurrentContext", "CGLDestroyContext"]) {
            if let Err(e) = cgl_check(result, what) {
                log::warn!("{e:#}");
            }
        }
    }
}

#[expect(unsafe_code, reason = "OpenGL calls")]
fn compile(kind: GLenum, source: &str) -> Result<GLuint> {
    let source = CString::new(source)?;
    // SAFETY: a context is current on this thread (GlContext::new_current); `source`
    // outlives the call, and the log buffer is sized from INFO_LOG_LENGTH.
    unsafe {
        let shader = gl::CreateShader(kind);
        gl::ShaderSource(shader, 1, &source.as_ptr(), std::ptr::null());
        gl::CompileShader(shader);
        let mut ok: GLint = 0;
        gl::GetShaderiv(shader, gl::COMPILE_STATUS, &mut ok);
        if ok == 0 {
            let mut length: GLint = 0;
            gl::GetShaderiv(shader, gl::INFO_LOG_LENGTH, &mut length);
            let mut log = vec![0u8; usize::try_from(length).unwrap_or(0).max(1)];
            gl::GetShaderInfoLog(shader, length, std::ptr::null_mut(), log.as_mut_ptr().cast::<GLchar>());
            gl::DeleteShader(shader);
            bail!("shader compile failed: {}", String::from_utf8_lossy(&log));
        }
        Ok(shader)
    }
}

#[expect(unsafe_code, reason = "OpenGL calls")]
fn link(fragment_source: &str) -> Result<GLuint> {
    let vertex = compile(gl::VERTEX_SHADER, VERTEX_SHADER)?;
    let fragment = match compile(gl::FRAGMENT_SHADER, fragment_source) {
        Ok(fragment) => fragment,
        Err(e) => {
            // SAFETY: `vertex` was created above on this thread's current context.
            unsafe { gl::DeleteShader(vertex) };
            return Err(e);
        }
    };
    // SAFETY: a context is current on this thread, and both shaders are valid.
    unsafe {
        let program = gl::CreateProgram();
        gl::AttachShader(program, vertex);
        gl::AttachShader(program, fragment);
        gl::LinkProgram(program);
        // Flagged for deletion; they are freed together with the program.
        gl::DeleteShader(vertex);
        gl::DeleteShader(fragment);
        let mut ok: GLint = 0;
        gl::GetProgramiv(program, gl::LINK_STATUS, &mut ok);
        if ok == 0 {
            gl::DeleteProgram(program);
            bail!("shader link failed");
        }
        gl::UseProgram(program);
        gl::Uniform1i(gl::GetUniformLocation(program, c"rgb".as_ptr()), 0);
        Ok(program)
    }
}

fn pixel_buffer_pool(width: u32, height: u32) -> Result<CVPixelBufferPool> {
    let key = |key: CVPixelBufferKeys| CFString::from(key);
    let no_properties: CFDictionary<CFString, CFType> = CFDictionary::from_CFType_pairs(&[]);
    let attributes = CFDictionary::from_CFType_pairs(&[
        (
            key(CVPixelBufferKeys::PixelFormatType),
            CFNumber::from(i64::from(kCVPixelFormatType_420YpCbCr8BiPlanarFullRange)).as_CFType(),
        ),
        (key(CVPixelBufferKeys::Width), CFNumber::from(i64::from(width)).as_CFType()),
        (key(CVPixelBufferKeys::Height), CFNumber::from(i64::from(height)).as_CFType()),
        // An (empty) IOSurface properties dictionary makes the buffers IOSurface-backed.
        (key(CVPixelBufferKeys::IOSurfaceProperties), no_properties.as_CFType()),
        (key(CVPixelBufferKeys::MetalCompatibility), CFBoolean::true_value().as_CFType()),
        (key(CVPixelBufferKeys::OpenGLCompatibility), CFBoolean::true_value().as_CFType()),
    ]);
    CVPixelBufferPool::new(None, Some(&attributes)).map_err(|code| anyhow!("CVPixelBufferPoolCreate failed ({code})"))
}

/// The RGB target mpv renders into, plus the passes that turn it into NV12.
pub struct Nv12Converter {
    context: CGLContextObj,
    vertex_array: GLuint,
    luma_program: GLuint,
    chroma_program: GLuint,
    rgb_texture: GLuint,
    rgb_framebuffer: GLuint,
    plane_textures: [GLuint; 2],
    plane_framebuffer: GLuint,
    size: (u32, u32),
    pool: Option<CVPixelBufferPool>,
}

impl Nv12Converter {
    /// Create the GL objects on `context`, which must be current on this thread.
    #[expect(unsafe_code, reason = "OpenGL calls")]
    pub fn new(context: &GlContext) -> Result<Self> {
        let mut this = Self {
            context: context.0,
            vertex_array: 0,
            luma_program: 0,
            chroma_program: 0,
            rgb_texture: 0,
            rgb_framebuffer: 0,
            plane_textures: [0; 2],
            plane_framebuffer: 0,
            size: (0, 0),
            pool: None,
        };
        // Drop deletes whatever got created if a later step fails.
        this.luma_program = link(LUMA_SHADER)?;
        this.chroma_program = link(CHROMA_SHADER)?;
        // SAFETY: `context` is current on this thread; every out-pointer is a live field.
        unsafe {
            gl::GenVertexArrays(1, &mut this.vertex_array);
            gl::GenTextures(1, &mut this.rgb_texture);
            gl::GenTextures(2, this.plane_textures.as_mut_ptr());
            gl::GenFramebuffers(1, &mut this.rgb_framebuffer);
            gl::GenFramebuffers(1, &mut this.plane_framebuffer);
            gl::BindTexture(gl::TEXTURE_2D, this.rgb_texture);
            for (parameter, value) in [
                (gl::TEXTURE_MIN_FILTER, gl::LINEAR),
                (gl::TEXTURE_MAG_FILTER, gl::LINEAR),
                (gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE),
                (gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE),
            ] {
                gl::TexParameteri(gl::TEXTURE_2D, parameter, value as GLint);
            }
            gl_error("creating the NV12 converter")?;
        }
        Ok(this)
    }

    /// The framebuffer mpv should render the next `width` x `height` frame into
    /// (both even). Reallocates the target on a size change; the buffer pool is
    /// recreated lazily by [`Self::convert`], so discard-only targets never get one.
    #[expect(unsafe_code, reason = "OpenGL calls")]
    pub fn target(&mut self, width: u32, height: u32) -> Result<GLuint> {
        if self.size != (width, height) {
            self.pool = None;
            self.size = (0, 0);
            let (gl_width, gl_height) = (GLint::try_from(width)?, GLint::try_from(height)?);
            // SAFETY: the context is current; a null data pointer only allocates storage.
            unsafe {
                gl::BindTexture(gl::TEXTURE_2D, self.rgb_texture);
                gl::TexImage2D(
                    gl::TEXTURE_2D,
                    0,
                    gl::RGBA8 as GLint,
                    gl_width,
                    gl_height,
                    0,
                    gl::RGBA,
                    gl::UNSIGNED_BYTE,
                    std::ptr::null(),
                );
                gl::BindFramebuffer(gl::FRAMEBUFFER, self.rgb_framebuffer);
                gl::FramebufferTexture2D(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::TEXTURE_2D, self.rgb_texture, 0);
                framebuffer_complete("RGB target")?;
                gl_error("allocating the RGB target")?;
            }
            self.size = (width, height);
        }
        Ok(self.rgb_framebuffer)
    }

    /// Write the frame mpv just rendered into a fresh NV12 pixel buffer and wait
    /// until the GPU has finished, so Metal can sample it immediately.
    #[expect(unsafe_code, reason = "OpenGL calls")]
    pub fn convert(&mut self) -> Result<CVPixelBuffer> {
        let (width, height) = self.size;
        if width == 0 || height == 0 {
            bail!("convert() called before target()");
        }
        let pool = match &self.pool {
            Some(pool) => pool,
            None => self.pool.insert(pixel_buffer_pool(width, height)?),
        };
        let buffer = pool
            .create_pixel_buffer()
            .map_err(|code| anyhow!("CVPixelBufferPoolCreatePixelBuffer failed ({code})"))?;
        let drawn = self.draw_planes(&buffer);
        // mpv expects a near-default GL state when it renders the next frame.
        // SAFETY: the context is current; binding 0 / the default unit is always valid.
        unsafe {
            gl::BindTexture(gl::TEXTURE_RECTANGLE, 0);
            gl::ActiveTexture(gl::TEXTURE0);
            gl::BindTexture(gl::TEXTURE_2D, 0);
            gl::UseProgram(0);
            gl::BindVertexArray(0);
            gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
        }
        drawn?;
        // SAFETY: the context is current; the fence is created, waited on and deleted here.
        unsafe {
            let fence = gl::FenceSync(gl::SYNC_GPU_COMMANDS_COMPLETE, 0);
            let waited = gl::ClientWaitSync(fence, gl::SYNC_FLUSH_COMMANDS_BIT, 1_000_000_000);
            gl::DeleteSync(fence);
            if waited != gl::ALREADY_SIGNALED && waited != gl::CONDITION_SATISFIED {
                bail!("GPU did not finish the frame (glClientWaitSync = {waited:#x})");
            }
        }
        Ok(buffer)
    }

    #[expect(unsafe_code, reason = "OpenGL and CGL IOSurface calls")]
    fn draw_planes(&self, buffer: &CVPixelBuffer) -> Result<()> {
        let (width, height) = self.size;
        // Not `CVPixelBuffer::get_io_surface`: core-video 0.4.3 wraps this
        // get-rule reference as owned, and its drop over-releases the IOSurface.
        // SAFETY: `buffer` is a valid pixel buffer; the returned reference is
        // borrowed and stays valid while `buffer` is alive.
        let surface_ref = unsafe { CVPixelBufferGetIOSurface(buffer.as_concrete_TypeRef()) };
        if surface_ref.is_null() {
            bail!("pixel buffer is not IOSurface-backed");
        }
        let surface_ref = surface_ref.cast_mut().cast::<c_void>();
        let planes = [
            (0, self.luma_program, gl::R8, gl::RED, width, height),
            (1, self.chroma_program, gl::RG8, gl::RG, width / 2, height / 2),
        ];
        // SAFETY: the context is current on this thread; the caller keeps `buffer`
        // (and so the IOSurface) alive and waits for the GPU before handing it out.
        unsafe {
            gl::BindFramebuffer(gl::FRAMEBUFFER, self.plane_framebuffer);
            gl::BindVertexArray(self.vertex_array);
            gl::ActiveTexture(gl::TEXTURE0);
            gl::BindTexture(gl::TEXTURE_2D, self.rgb_texture);
            gl::ActiveTexture(gl::TEXTURE1);
            for ((plane, program, internal_format, format, plane_width, plane_height), texture) in
                planes.into_iter().zip(self.plane_textures)
            {
                let (plane_width, plane_height) = (GLint::try_from(plane_width)?, GLint::try_from(plane_height)?);
                gl::BindTexture(gl::TEXTURE_RECTANGLE, texture);
                cgl_check(
                    CGLTexImageIOSurface2D(
                        self.context,
                        gl::TEXTURE_RECTANGLE,
                        internal_format,
                        plane_width,
                        plane_height,
                        format,
                        gl::UNSIGNED_BYTE,
                        surface_ref,
                        plane,
                    ),
                    "CGLTexImageIOSurface2D",
                )?;
                gl::FramebufferTexture2D(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::TEXTURE_RECTANGLE, texture, 0);
                framebuffer_complete("NV12 plane")?;
                gl::Viewport(0, 0, plane_width, plane_height);
                gl::UseProgram(program);
                gl::DrawArrays(gl::TRIANGLES, 0, 3);
            }
        }
        gl_error("converting to NV12")
    }
}

impl Drop for Nv12Converter {
    #[expect(unsafe_code, reason = "OpenGL calls")]
    fn drop(&mut self) {
        // SAFETY: the context is still current (the GlContext outlives this converter),
        // and deleting the name 0 is a no-op.
        unsafe {
            gl::DeleteFramebuffers(1, &self.rgb_framebuffer);
            gl::DeleteFramebuffers(1, &self.plane_framebuffer);
            gl::DeleteTextures(1, &self.rgb_texture);
            gl::DeleteTextures(2, self.plane_textures.as_ptr());
            gl::DeleteVertexArrays(1, &self.vertex_array);
            gl::DeleteProgram(self.luma_program);
            gl::DeleteProgram(self.chroma_program);
        }
    }
}

#[expect(unsafe_code, reason = "OpenGL calls")]
fn framebuffer_complete(what: &str) -> Result<()> {
    // SAFETY: only queries the bound framebuffer of the current context.
    let status = unsafe { gl::CheckFramebufferStatus(gl::FRAMEBUFFER) };
    if status != gl::FRAMEBUFFER_COMPLETE {
        bail!("{what} framebuffer incomplete ({status:#x})");
    }
    Ok(())
}

#[expect(unsafe_code, reason = "OpenGL calls")]
fn gl_error(what: &str) -> Result<()> {
    // SAFETY: only reads the current context's error flag.
    let error = unsafe { gl::GetError() };
    if error != gl::NO_ERROR {
        bail!("OpenGL error {error:#x} while {what}");
    }
    Ok(())
}
