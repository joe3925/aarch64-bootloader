#![no_std]
#![cfg_attr(not(test), no_main)]

pub mod cfg;
pub mod framebuffer;

use crate::cfg::CfgFile;
use crate::cfg::FromCfg;
pub use crate::framebuffer::{FrameBuffer, FrameBufferConfig, FrameBufferInfo, PixelFormat};
pub use aarch64_vmsa::address::GranuleKind;

#[derive(Debug)]
#[repr(C)]
pub struct BootInfo {
    pub memory_map: MemoryMap,
    pub framebuffer: Optional<FrameBuffer>,
    pub rsdp_addr: u64,
    pub fdt_addr: u64,
    pub stub_entry: u64,
    pub stub_virt_base: u64,
    pub stub_virt_size: u64,
    pub translation: TranslationInfo,
}

#[derive(Debug)]
#[repr(C)]
pub enum Optional<T> {
    Some(T),
    None,
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct TranslationInfo {
    pub root_table: u64,
    pub recursive_base: u64,
    pub recursive_index: usize,
    pub input_addr_bits: u8,
    pub output_addr_bits: u8,
    pub granule_kind: GranuleKind,
    pub mair_el1: u64,
    pub tcr_el1: u64,
    pub tcr2_el1: u64,
    pub ttbr1_el1: u64,
    pub scratch_page: u64,
    pub scratch_descriptor: *mut u64,
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct MemoryMap {
    buffer: *const u8,
    buffer_len: usize,
    descriptor_size: usize,
    descriptor_version: u32,
}

impl MemoryMap {
    pub const unsafe fn from_raw_parts(
        buffer: *const u8,
        buffer_len: usize,
        descriptor_size: usize,
        descriptor_version: u32,
    ) -> Self {
        Self {
            buffer,
            buffer_len,
            descriptor_size,
            descriptor_version,
        }
    }

    pub const fn buffer(&self) -> *const u8 {
        self.buffer
    }

    pub const fn buffer_len(&self) -> usize {
        self.buffer_len
    }

    pub const fn descriptor_size(&self) -> usize {
        self.descriptor_size
    }

    pub const fn descriptor_version(&self) -> u32 {
        self.descriptor_version
    }

    pub const fn len(&self) -> usize {
        if self.descriptor_size == 0 {
            0
        } else {
            self.buffer_len / self.descriptor_size
        }
    }

    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn iter(&self) -> MemoryMapIter<'_> {
        MemoryMapIter {
            map: self,
            index: 0,
        }
    }
}

pub struct MemoryMapIter<'a> {
    map: &'a MemoryMap,
    index: usize,
}

impl Iterator for MemoryMapIter<'_> {
    type Item = UefiMemoryDescriptor;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.map.len()
            || self.map.descriptor_size < core::mem::size_of::<UefiMemoryDescriptor>()
        {
            return None;
        }
        let offset = self.index.checked_mul(self.map.descriptor_size)?;
        self.index += 1;
        Some(unsafe {
            (self.map.buffer.add(offset) as *const UefiMemoryDescriptor).read_unaligned()
        })
    }
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct UefiMemoryDescriptor {
    pub memory_type: u32,
    pub padding: u32,
    pub phys_start: u64,
    pub virt_start: u64,
    pub page_count: u64,
    pub attributes: u64,
}

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
