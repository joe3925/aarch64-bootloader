#![no_std]
#![no_main]

extern crate alloc;

pub mod file;
pub mod framebuffer;
pub mod kernel_loading;
pub mod mapper;
use crate::file::read_file;
use crate::kernel_loading::{load_kernel_at_preferred_virtual_address, validate_kernel};
use crate::mapper::BootMapper;
use bootloader_api::BootConfig;
use bootloader_api::cfg::{CfgFile, FromCfg};
use core::ffi::c_void;
use goblin::elf::Elf;
use uefi::Guid;
use uefi::boot::{self, ScopedProtocol};
use uefi::fs::FileSystem;
use uefi::guid;
use uefi::helpers;
use uefi::prelude::*;
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::system::with_config_table;
use uefi::table::cfg::ConfigTableEntry;

const CFG_PATH: &str = "/EFI/BOOT/boot.cfg";
const EFI_DTB_TABLE_GUID: Guid = guid!("b1b621d5-f19c-41a5-830b-d9152c69aae0");
#[entry]
fn main() -> Status {
    match init() {
        Ok(()) => Status::SUCCESS,
        Err(err) => err.status(),
    }
}

fn init() -> Result<(), BootError> {
    helpers::init().map_err(|_| BootError::UefiInit)?;

    let mut rdsp: Option<*const c_void> = None;
    let mut fdt: Option<*const c_void> = None;
    with_config_table(|entries| {
        for entry in entries {
            match entry.guid {
                ConfigTableEntry::ACPI2_GUID => {
                    rdsp = Some(entry.address);
                    continue;
                }
                ConfigTableEntry::ACPI_GUID => {
                    if rdsp.is_none() {
                        rdsp = Some(entry.address)
                    } else {
                        continue;
                    }
                }
                EFI_DTB_TABLE_GUID => {
                    fdt = Some(entry.address);
                    continue;
                }
                _ => continue,
            }
        }
    });

    let fs = boot::get_image_file_system(boot::image_handle())
        .map_err(|_| BootError::ImageFileSystem)?;

    let mut fs = FileSystem::new(fs);

    let cfg_file = read_file(CFG_PATH, &mut fs).map_err(|_| BootError::CfgRead)?;
    let cfg = CfgFile::parse(&cfg_file).map_err(|_| BootError::CfgParse)?;
    let boot_cfg = BootConfig::from_cfg(&cfg).map_err(|_| BootError::BootConfig)?;
    let _framebuffer =
        framebuffer::framebuffer(&boot_cfg.framebuffer).map_err(|_| BootError::Framebuffer)?;

    let kernel_binary = read_file(boot_cfg.kernel, &mut fs).map_err(|_| BootError::KernelRead)?;

    let kernel = Elf::parse(&kernel_binary).map_err(|_| BootError::KernelParse)?;

    validate_kernel(&kernel).map_err(|_| BootError::KernelInvalid)?;
    let mut mapper = BootMapper {};
    let loaded_kernel =
        load_kernel_at_preferred_virtual_address(&kernel, &kernel_binary, &mut mapper)
            .map_err(|_| BootError::KernelLoad)?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BootError {
    UefiInit,
    ImageFileSystem,
    CfgRead,
    CfgParse,
    BootConfig,
    Framebuffer,
    KernelRead,
    KernelParse,
    KernelInvalid,
    KernelLoad,
}

impl BootError {
    fn status(self) -> Status {
        match self {
            Self::UefiInit => Status::ABORTED,
            Self::ImageFileSystem => Status::UNSUPPORTED,
            Self::CfgRead => Status::NOT_FOUND,
            Self::CfgParse => Status::LOAD_ERROR,
            Self::BootConfig => Status::LOAD_ERROR,
            Self::Framebuffer => Status::UNSUPPORTED,
            Self::KernelRead => Status::NOT_FOUND,
            Self::KernelParse => Status::LOAD_ERROR,
            Self::KernelInvalid => Status::LOAD_ERROR,
            Self::KernelLoad => Status::LOAD_ERROR,
        }
    }
}
