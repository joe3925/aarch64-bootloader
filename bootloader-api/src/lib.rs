#![no_std]
#![cfg_attr(not(test), no_main)]

pub mod cfg;
pub mod framebuffer;

use crate::cfg::CfgFile;
use crate::cfg::FromCfg;
pub use crate::framebuffer::{FrameBuffer, FrameBufferConfig, FrameBufferInfo, PixelFormat};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BootConfig<'a> {
    pub kernel: &'a str,
    pub framebuffer: FrameBufferConfig,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootConfigError {
    MissingKernel,
    MissingFramebufferWidth,
    InvalidFramebufferWidth,
    MissingFramebufferHeight,
    InvalidFramebufferHeight,
}

impl<'a> FromCfg<'a> for BootConfig<'a> {
    type Error = BootConfigError;

    fn from_cfg(cfg: &CfgFile<'a>) -> Result<Self, Self::Error> {
        let kernel = cfg.get("kernel").ok_or(BootConfigError::MissingKernel)?;
        let framebuffer_width = cfg
            .get("framebuffer_width")
            .ok_or(BootConfigError::MissingFramebufferWidth)?
            .parse()
            .map_err(|_| BootConfigError::InvalidFramebufferWidth)?;
        let framebuffer_height = cfg
            .get("framebuffer_height")
            .ok_or(BootConfigError::MissingFramebufferHeight)?
            .parse()
            .map_err(|_| BootConfigError::InvalidFramebufferHeight)?;

        Ok(Self {
            kernel,
            framebuffer: FrameBufferConfig {
                minimum_width: Some(framebuffer_width),
                minimum_height: Some(framebuffer_height),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_framebuffer_dimensions() {
        let cfg = CfgFile::parse(
            b"kernel=/kernel.elf\nframebuffer_width=1920\nframebuffer_height=1080\n",
        )
        .unwrap();

        let config = BootConfig::from_cfg(&cfg).unwrap();

        assert_eq!(config.kernel, "/kernel.elf");
        assert_eq!(config.framebuffer.minimum_width, Some(1920));
        assert_eq!(config.framebuffer.minimum_height, Some(1080));
    }

    #[test]
    fn rejects_invalid_framebuffer_dimensions() {
        let cfg = CfgFile::parse(
            b"kernel=/kernel.elf\nframebuffer_width=wide\nframebuffer_height=1080\n",
        )
        .unwrap();

        assert_eq!(
            BootConfig::from_cfg(&cfg),
            Err(BootConfigError::InvalidFramebufferWidth)
        );
    }
}
