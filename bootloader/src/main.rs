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
    BootConfig as MapperConfig, BootMapperInvalidation, BootPlanner, UnavailableTableProvider,
};
use aarch64_cpu::registers::{MAIR_EL1, TCR_EL1, TTBR1_EL1};
use aarch64_vmsa::address::{TranslationGranule, VirtAddr};
use aarch64_vmsa::config::format::Vmsa64;
use aarch64_vmsa::config::granule::Granule4KiB;
use bootloader_api::cfg::{CfgFile, FromCfg};
use bootloader_api::{
    BootConfig, BootInfo, MemoryMap as BootMemoryMap, Optional, TranslationInfo,
};
use core::arch::asm;
use core::ffi::c_void;
use goblin::elf::Elf;
use tock_registers::interfaces::Readable;
use uefi::Guid;
use uefi::boot;
use uefi::boot::AllocateType;
use uefi::fs::FileSystem;
use uefi::guid;
use uefi::helpers;
use uefi::mem::memory_map::{MemoryMap, MemoryType};
use uefi::prelude::*;
use uefi::proto::loaded_image::LoadedImage;
use uefi::system::with_config_table;
use uefi::table::cfg::ConfigTableEntry;

const CFG_PATH: &str = env!("RUSTOS_BOOT_CONFIG_PATH");
const EFI_DTB_TABLE_GUID: Guid = guid!("b1b621d5-f19c-41a5-830b-d9152c69aae0");
const RECURSIVE_INDEX: usize = 510;
const SCRATCH_PAGE: u64 = 0xFFFF_8300_0000_0000;
#[entry]
fn main() -> Status {
    match init() {
        Ok(()) => Status::SUCCESS,
        Err(err) => {
            uefi::println!("rustOS boot error: {err:?}");
            err.status()
        }
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
    let framebuffer = match framebuffer::framebuffer(&boot_cfg.framebuffer) {
        Ok(framebuffer) => Optional::Some(framebuffer),
        Err(_) => Optional::None,
    };

    let kernel_binary = read_file(boot_cfg.kernel, &mut fs).map_err(|_| BootError::KernelRead)?;

    let kernel = Elf::parse(&kernel_binary).map_err(|_| BootError::KernelParse)?;

    validate_kernel(&kernel).map_err(|_| BootError::KernelInvalid)?;
    let output_addr_bits = match (TCR_EL1.get() >> 32) & 0x7 {
        0 => 32,
        1 => 36,
        2 => 40,
        3 => 42,
        4 => 44,
        5 => 48,
        6 => 52,
        _ => return Err(BootError::MapperInit),
    };
    let mut mapper = BootPlanner::<Vmsa64, Granule4KiB>::new(
        MapperConfig::default(),
        48,
        output_addr_bits,
    )
        .map_err(|_| BootError::MapperInit)?;
    let (mapping, mapping_config) = mapper.mapping_parts_mut();
    let loaded_kernel =
        load_kernel_at_preferred_virtual_address(&kernel, &kernel_binary, mapping, mapping_config)
            .map_err(BootError::KernelLoad)?;
    drop(fs);

    let loaded_image = boot::open_protocol_exclusive::<LoadedImage>(boot::image_handle())
        .map_err(|_| BootError::LoadedImage)?;
    let (image_base, image_size) = loaded_image.info();
    let image_start = image_base as u64;
    let image_end = image_start
        .checked_add(image_size)
        .ok_or(BootError::TransitionMapping)?;
    drop(loaded_image);

    let stub_stack = boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, 16)
        .map_err(|_| BootError::TransitionMapping)?;
    let stub_stack_top = stub_stack.as_ptr() as u64 + 16 * boot::PAGE_SIZE as u64;
    let kernel_stack = boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, 256)
        .map_err(|_| BootError::TransitionMapping)?;
    let kernel_stack_top = kernel_stack.as_ptr() as u64 + 256 * boot::PAGE_SIZE as u64;

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
    let root_table = root.addr().raw();
    let input_addr_bits = root.addr_bits();
    let output_addr_bits = root.output_addr_bits();
    mapper
        .mapping_parts_mut()
        .0
        .map_kernel_range(
            &MapperConfig::default(),
            SCRATCH_PAGE,
            root_table,
            Granule4KiB::SIZE,
            data_permissions(),
        )
        .map_err(|_| BootError::TransitionMapping)?;
    let recursive_access = unsafe {
        aarch64_vmsa::table::RecursiveTableAccess::<Vmsa64, Granule4KiB>::new(
            RECURSIVE_INDEX,
            VirtAddr(recursive_base),
            root.addr(),
            root.level(),
        )
    }
    .map_err(|_| BootError::RecursiveMapping)?;
    let scratch_descriptor = recursive_descriptor(recursive_base, SCRATCH_PAGE);
    unsafe {
        asm!("msr daifset, #0xf", options(nomem, nostack, preserves_flags));
    }
    let memory_map = unsafe { boot::exit_boot_services(Some(MemoryType::LOADER_DATA)) };
    let meta = memory_map.meta();
    let memory_map_buffer = memory_map.buffer();
    let memory_map_len = memory_map.len() * meta.desc_size;
    let boot_memory_map = unsafe {
        BootMemoryMap::from_raw_parts(
            memory_map_buffer.as_ptr(),
            memory_map_len,
            meta.desc_size,
            meta.desc_version,
        )
    };
    core::mem::forget(memory_map);

    let _online_mapper = match unsafe {
        mapper.install(recursive_access, UnavailableTableProvider, BootMapperInvalidation)
    } {
        Ok(mapper) => mapper,
        Err(_) => {
            debug_message("bootloader: mapper install failed\r\n");
            loop {
                core::hint::spin_loop();
            }
        }
    };
    let tcr2_el1 = 0;
    let boot_info = BootInfo {
        memory_map: boot_memory_map,
        framebuffer,
        rsdp_addr: rdsp.map_or(0, |address| address as u64),
        fdt_addr: fdt.map_or(0, |address| address as u64),
        stub_entry: loaded_kernel.virt_entry,
        stub_virt_base: loaded_kernel.virt_base,
        stub_virt_size: loaded_kernel.virt_size,
        translation: TranslationInfo {
            root_table,
            recursive_base,
            recursive_index: RECURSIVE_INDEX,
            input_addr_bits,
            output_addr_bits,
            granule_kind: Granule4KiB::KIND,
            mair_el1: MAIR_EL1.get(),
            tcr_el1: TCR_EL1.get(),
            tcr2_el1,
            ttbr1_el1: TTBR1_EL1.get(),
            scratch_page: SCRATCH_PAGE,
            scratch_descriptor: scratch_descriptor as *mut u64,
        },
    };
    unsafe {
        enter_stub(
            loaded_kernel.virt_entry,
            &boot_info,
            stub_stack_top,
            kernel_stack_top,
        )
    }
}

pub(crate) fn debug_message(message: &str) {
    let uart = 0x0900_0000 as *mut u32;
    for byte in message.bytes() {
        unsafe {
            while uart.add(6).read_volatile() & (1 << 5) != 0 {
                core::hint::spin_loop();
            }
            uart.write_volatile(byte as u32);
        }
    }
}

fn recursive_descriptor(recursive_base: u64, virtual_address: u64) -> u64 {
    let index_mask = 0x1ff;
    let l0 = (virtual_address >> 39) & index_mask;
    let l1 = (virtual_address >> 30) & index_mask;
    let l2 = (virtual_address >> 21) & index_mask;
    let l3 = (virtual_address >> 12) & index_mask;
    (recursive_base & 0xffff_ff80_0000_0000)
        | (l0 << 30)
        | (l1 << 21)
        | (l2 << 12)
        | (l3 * core::mem::size_of::<u64>() as u64)
}

unsafe fn enter_stub(
    entry: u64,
    boot_info: *const BootInfo,
    stack_top: u64,
    kernel_stack_top: u64,
) -> ! {
    unsafe {
        asm!(
            "mov sp, {stack_top}",
            "br {entry}",
            in("x0") boot_info,
            in("x1") kernel_stack_top,
            entry = in(reg) entry,
            stack_top = in(reg) stack_top,
            options(noreturn)
        )
    }
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

#[derive(Debug)]
enum BootError {
    UefiInit,
    ImageFileSystem,
    CfgRead,
    CfgParse,
    BootConfig,
    KernelRead,
    KernelParse,
    KernelInvalid,
    KernelLoad(kernel_loading::KernelLoadError<mapper::MapperError>),
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
            Self::KernelRead => Status::NOT_FOUND,
            Self::KernelParse => Status::LOAD_ERROR,
            Self::KernelInvalid => Status::LOAD_ERROR,
            Self::KernelLoad(_) => Status::LOAD_ERROR,
            Self::MapperInit => Status::OUT_OF_RESOURCES,
            Self::LoadedImage => Status::LOAD_ERROR,
            Self::TransitionMapping => Status::LOAD_ERROR,
            Self::RecursiveMapping => Status::LOAD_ERROR,
            Self::MapperInstall => Status::LOAD_ERROR,
        }
    }
}
