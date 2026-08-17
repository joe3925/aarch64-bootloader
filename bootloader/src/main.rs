#![no_std]
#![no_main]

extern crate alloc;

pub mod file;
pub mod framebuffer;
pub mod kernel_loading;
pub mod mapper;
use crate::file::read_file;
use crate::kernel_loading::SegmentPerms;
use crate::kernel_loading::{load_kernel_at_preferred_virtual_address, validate_kernel};
use crate::mapper::{
    BootConfig as MapperConfig, BootMapperInvalidation, BootPlanner, FixedTablePool,
};
use aarch64_vmsa::address::VirtAddr;
use aarch64_vmsa::config::format::Vmsa64;
use aarch64_vmsa::config::granule::Granule4KiB;
use bootloader_api::BootConfig;
use bootloader_api::cfg::{CfgFile, FromCfg};
use core::ffi::c_void;
use goblin::elf::Elf;
use uefi::Guid;
use uefi::boot::{self, ScopedProtocol};
use uefi::fs::FileSystem;
use uefi::guid;
use uefi::helpers;
use uefi::mem::memory_map::{MemoryMap, MemoryType};
use uefi::prelude::*;
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::system::with_config_table;
use uefi::table::cfg::ConfigTableEntry;

const CFG_PATH: &str = "/EFI/BOOT/boot.cfg";
const EFI_DTB_TABLE_GUID: Guid = guid!("b1b621d5-f19c-41a5-830b-d9152c69aae0");
const RECURSIVE_INDEX: usize = 510;
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
    let mut mapper = BootPlanner::<Vmsa64, Granule4KiB>::new(MapperConfig::default(), 48, 48)
        .map_err(|_| BootError::MapperInit)?;
    let (mapping, mapping_config) = mapper.mapping_parts_mut();
    let loaded_kernel =
        load_kernel_at_preferred_virtual_address(&kernel, &kernel_binary, mapping, mapping_config)
            .map_err(|_| BootError::KernelLoad)?;
    drop(fs);

    let loaded_image = boot::open_protocol_exclusive::<LoadedImage>(boot::image_handle())
        .map_err(|_| BootError::LoadedImage)?;
    let (image_base, image_size) = loaded_image.info();
    let image_start = image_base as u64;
    let image_end = image_start
        .checked_add(image_size)
        .ok_or(BootError::TransitionMapping)?;
    drop(loaded_image);

    let live_pool = FixedTablePool::<Granule4KiB>::new(16).map_err(|_| BootError::MapperInit)?;
    let memory_map =
        boot::memory_map(MemoryType::LOADER_DATA).map_err(|_| BootError::TransitionMapping)?;
    let (mapping, mapping_config) = mapper.mapping_parts_mut();
    for descriptor in memory_map.entries() {
        if !transition_memory_type(descriptor.ty) || descriptor.page_count == 0 {
            continue;
        }
        let start = descriptor.phys_start;
        let len = descriptor
            .page_count
            .checked_mul(boot::PAGE_SIZE as u64)
            .ok_or(BootError::TransitionMapping)?;
        let end = start.checked_add(len).ok_or(BootError::TransitionMapping)?;

        map_transition_part(
            mapping,
            mapping_config,
            start,
            core::cmp::min(end, image_start),
            data_permissions(),
        )?;
        map_transition_part(
            mapping,
            mapping_config,
            core::cmp::max(start, image_start),
            core::cmp::min(end, image_end),
            image_permissions(),
        )?;
        map_transition_part(
            mapping,
            mapping_config,
            core::cmp::max(start, image_end),
            end,
            data_permissions(),
        )?;
    }

    let recursive_base = mapper
        .mapping_parts_mut()
        .0
        .install_recursive_mapping(RECURSIVE_INDEX)
        .map_err(|_| BootError::RecursiveMapping)?;
    let root = mapper.root();
    let recursive_access = unsafe {
        aarch64_vmsa::table::RecursiveTableAccess::<Vmsa64, Granule4KiB>::new(
            RECURSIVE_INDEX,
            VirtAddr(recursive_base),
            root.addr(),
            root.level(),
        )
    }
    .map_err(|_| BootError::RecursiveMapping)?;
    let _online_mapper =
        unsafe { mapper.install(recursive_access, live_pool, BootMapperInvalidation) }
            .map_err(|_| BootError::MapperInstall)?;
    let _ = loaded_kernel;
    Ok(())
}

fn map_transition_part(
    mapping: &mut crate::mapper::MappingPrimitive<
        Vmsa64,
        Granule4KiB,
        crate::mapper::IdentityTableAccess<Vmsa64, Granule4KiB>,
        crate::mapper::UefiTablePool<Granule4KiB>,
        aarch64_vmsa::mapper::Offline,
    >,
    config: &MapperConfig,
    start: u64,
    end: u64,
    permissions: SegmentPerms,
) -> Result<(), BootError> {
    if start >= end {
        return Ok(());
    }
    mapping
        .map_kernel_range(config, start, start, end - start, permissions)
        .map_err(|_| BootError::TransitionMapping)
}

fn transition_memory_type(memory_type: MemoryType) -> bool {
    matches!(
        memory_type,
        MemoryType::LOADER_CODE
            | MemoryType::LOADER_DATA
            | MemoryType::BOOT_SERVICES_CODE
            | MemoryType::BOOT_SERVICES_DATA
            | MemoryType::RUNTIME_SERVICES_CODE
            | MemoryType::RUNTIME_SERVICES_DATA
            | MemoryType::ACPI_RECLAIM
            | MemoryType::ACPI_NON_VOLATILE
    )
}

const fn data_permissions() -> SegmentPerms {
    SegmentPerms {
        read: true,
        write: true,
        execute: false,
    }
}
const fn image_permissions() -> SegmentPerms {
    SegmentPerms {
        read: true,
        write: true,
        execute: true,
    }
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
    MapperInit,
    LoadedImage,
    TransitionMapping,
    RecursiveMapping,
    MapperInstall,
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
            Self::MapperInit => Status::OUT_OF_RESOURCES,
            Self::LoadedImage => Status::LOAD_ERROR,
            Self::TransitionMapping => Status::LOAD_ERROR,
            Self::RecursiveMapping => Status::LOAD_ERROR,
            Self::MapperInstall => Status::LOAD_ERROR,
        }
    }
}
