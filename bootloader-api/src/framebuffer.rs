use core::slice;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FrameBufferConfig {
    pub minimum_width: Option<usize>,
    pub minimum_height: Option<usize>,
}

#[derive(Debug)]
#[repr(C)]
pub struct FrameBuffer {
    buffer_start: u64,
    info: FrameBufferInfo,
}

impl FrameBuffer {
    pub const unsafe fn new(buffer_start: u64, info: FrameBufferInfo) -> Self {
        Self { buffer_start, info }
    }

    pub const fn buffer_start(&self) -> u64 {
        self.buffer_start
    }

    pub const fn info(&self) -> FrameBufferInfo {
        self.info
    }

    pub fn buffer(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.buffer_start as *const u8, self.info.byte_len) }
    }

    pub fn buffer_mut(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut(self.buffer_start as *mut u8, self.info.byte_len) }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct FrameBufferInfo {
    pub byte_len: usize,
    pub width: usize,
    pub height: usize,
    pub pixel_format: PixelFormat,
    pub bytes_per_pixel: usize,
    pub stride: usize,
}

/// The byte or bit layout of a framebuffer pixel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub enum PixelFormat {
    /// Red, green, blue, then one reserved byte.
    Rgb,
    /// Blue, green, red, then one reserved byte.
    Bgr,
    /// A GOP-defined 32-bit channel layout.
    Bitmask {
        red: u32,
        green: u32,
        blue: u32,
        reserved: u32,
    },
}
