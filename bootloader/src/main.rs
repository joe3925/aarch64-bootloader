#![no_std]
#![no_main]

extern crate alloc;

pub mod file;
pub mod kernel_loading;
pub mod mapper;
use crate::file::read_file;
use crate::kernel_loading::{load_kernel_at_preferred_virtual_address, validate_kernel};
use crate::mapper::BootMapper;
use bootloader_api::BootConfig;
use bootloader_api::cfg::{CfgFile, FromCfg};
use goblin::elf::Elf;
use uefi::boot::{self, ScopedProtocol};
use uefi::fs::FileSystem;
use uefi::helpers;
use uefi::prelude::*;
use uefi::proto::media::fs::SimpleFileSystem;

const CFG_PATH: &str = "/EFI/BOOT/boot.cfg";

#[entry]
fn main() -> Status {
    match init() {
        Ok(()) => Status::SUCCESS,
        Err(err) => err.status(),
    }
}

fn init() -> Result<(), BootError> {
    helpers::init().map_err(|_| BootError::UefiInit)?;

    let fs = boot::get_image_file_system(boot::image_handle())
        .map_err(|_| BootError::ImageFileSystem)?;

    let mut fs = FileSystem::new(fs);

    let cfg_file = read_file(CFG_PATH, &mut fs).map_err(|_| BootError::CfgRead)?;
    let cfg = CfgFile::parse(&cfg_file).map_err(|_| BootError::CfgParse)?;
    let boot_cfg = BootConfig::from_cfg(&cfg).map_err(|_| BootError::BootConfig)?;

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
            Self::KernelRead => Status::NOT_FOUND,
            Self::KernelParse => Status::LOAD_ERROR,
            Self::KernelInvalid => Status::LOAD_ERROR,
            Self::KernelLoad => Status::LOAD_ERROR,
        }
    }
}
