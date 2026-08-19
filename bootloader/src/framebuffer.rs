use bootloader_api::framebuffer::{FrameBuffer, FrameBufferConfig, FrameBufferInfo, PixelFormat};
use uefi::boot;
use uefi::proto::console::gop::{GraphicsOutput, PixelFormat as GopPixelFormat};

const GOP_BYTES_PER_PIXEL: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameBufferError {
    GraphicsOutputNotFound,
    GraphicsOutputOpen,
    NoCompatibleMode,
    ModeSet,
    BltOnly,
    InvalidLayout,
}

pub fn framebuffer(config: &FrameBufferConfig) -> Result<FrameBuffer, FrameBufferError> {
    let handle = match boot::get_handle_for_protocol::<GraphicsOutput>() {
        Ok(handle) => handle,
        Err(_) => return Err(FrameBufferError::GraphicsOutputNotFound),
    };
    let mut gop = boot::open_protocol_exclusive::<GraphicsOutput>(handle)
        .map_err(|_| FrameBufferError::GraphicsOutputOpen)?;

    if config.minimum_width.is_some() || config.minimum_height.is_some() {
        let mode = select_mode(&gop, config).ok_or(FrameBufferError::NoCompatibleMode)?;
        gop.set_mode(&mode).map_err(|_| FrameBufferError::ModeSet)?;
    }

    parse_gop_framebuffer(&mut gop)
}

fn parse_gop_framebuffer(gop: &mut GraphicsOutput) -> Result<FrameBuffer, FrameBufferError> {
    let mode = gop.current_mode_info();
    let (width, height) = mode.resolution();
    let stride = mode.stride();
    let pixel_format = match mode.pixel_format() {
        GopPixelFormat::Rgb => PixelFormat::Rgb,
        GopPixelFormat::Bgr => PixelFormat::Bgr,
        GopPixelFormat::Bitmask => {
            let masks = mode
                .pixel_bitmask()
                .ok_or(FrameBufferError::InvalidLayout)?;
            PixelFormat::Bitmask {
                red: masks.red,
                green: masks.green,
                blue: masks.blue,
                reserved: masks.reserved,
            }
        }
        GopPixelFormat::BltOnly => return Err(FrameBufferError::BltOnly),
    };

    let minimum_byte_len = stride
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(GOP_BYTES_PER_PIXEL))
        .ok_or(FrameBufferError::InvalidLayout)?;
    let mut gop_framebuffer = gop.frame_buffer();
    let byte_len = gop_framebuffer.size();
    if width == 0 || height == 0 || stride < width || byte_len < minimum_byte_len {
        return Err(FrameBufferError::InvalidLayout);
    }

    let info = FrameBufferInfo {
        byte_len,
        width,
        height,
        pixel_format,
        bytes_per_pixel: GOP_BYTES_PER_PIXEL,
        stride,
    };
    let buffer_start = gop_framebuffer.as_mut_ptr() as usize as u64;

    Ok(unsafe { FrameBuffer::new(buffer_start, info) })
}

fn select_mode(
    gop: &GraphicsOutput,
    config: &FrameBufferConfig,
) -> Option<uefi::proto::console::gop::Mode> {
    gop.modes()
        .filter(|mode| mode.info().pixel_format() != GopPixelFormat::BltOnly)
        .filter(|mode| {
            let (width, height) = mode.info().resolution();
            config.minimum_width.is_none_or(|minimum| width >= minimum)
                && config
                    .minimum_height
                    .is_none_or(|minimum| height >= minimum)
        })
        .min_by_key(|mode| {
            let (width, height) = mode.info().resolution();
            width.saturating_mul(height)
        })
}
