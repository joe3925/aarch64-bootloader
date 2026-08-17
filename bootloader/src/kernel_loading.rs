extern crate alloc;

use crate::mapper::{MapperError, MappingPrimitive};
use aarch64_vmsa::address::TranslationGranule;
use aarch64_vmsa::attrs::{Stage1MemoryConfig, Stage1PermissionConfig};
use aarch64_vmsa::config::format::Vmsa64;
use aarch64_vmsa::config::regime::NonSecureEl1Stage1;
use aarch64_vmsa::descriptor::HasLayout;
use aarch64_vmsa::mapper::Offline;
use aarch64_vmsa::table::{TableAccessMut, TableFrameProvider};
use alloc::vec::Vec;
use core::cmp;
use core::ptr;
use goblin::elf::Elf;
use goblin::elf::program_header;
use goblin::elf64::header;
use uefi::Status;
use uefi::boot;
use uefi::boot::AllocateType;
use uefi::boot::MemoryType;
use uefi::boot::PAGE_SIZE;

const DEFAULT_VA_BITS: u32 = 48;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SegmentPerms {
    pub read: bool,
    pub write: bool,
    pub execute: bool,
}

#[derive(Clone, Debug)]
pub struct LoadedSegment {
    pub virt_start: u64,
    pub phys_start: u64,
    pub byte_len: u64,
    pub perms: SegmentPerms,
}

#[derive(Clone, Debug)]
pub struct LoadedKernel {
    pub virt_entry: u64,
    pub virt_base: u64,
    pub virt_size: u64,
    pub segments: Vec<LoadedSegment>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KernelLoadError<MapError> {
    MissingLoadSegment,
    InvalidSegmentSize,
    InvalidSegmentOffset,
    InvalidSegmentRange,
    InvalidVirtualRange,
    MappedSegmentOverlap,
    PhysicalAllocationFailed(Status),
    MappingFailed(MapError),
}

pub fn load_kernel_at_preferred_virtual_address<G, A, P, C>(
    kernel: &Elf,
    kernel_bytes: &[u8],
    mapper: &mut MappingPrimitive<Vmsa64, G, A, P, Offline>,
    config: &C,
) -> Result<LoadedKernel, KernelLoadError<MapperError>>
where
    G: TranslationGranule,
    A: TableAccessMut<Vmsa64, G>,
    P: TableFrameProvider<G>,
    C: Stage1MemoryConfig + Stage1PermissionConfig,
    Vmsa64: HasLayout<<NonSecureEl1Stage1 as aarch64_vmsa::regime::TranslationRegime>::Stage, G>,
{
    load_kernel_at_preferred_virtual_address_with_va_bits(
        kernel,
        kernel_bytes,
        mapper,
        config,
        DEFAULT_VA_BITS,
    )
}

pub fn load_kernel_at_preferred_virtual_address_with_va_bits<G, A, P, C>(
    kernel: &Elf,
    kernel_bytes: &[u8],
    mapper: &mut MappingPrimitive<Vmsa64, G, A, P, Offline>,
    config: &C,
    va_bits: u32,
) -> Result<LoadedKernel, KernelLoadError<MapperError>>
where
    G: TranslationGranule,
    A: TableAccessMut<Vmsa64, G>,
    P: TableFrameProvider<G>,
    C: Stage1MemoryConfig + Stage1PermissionConfig,
    Vmsa64: HasLayout<<NonSecureEl1Stage1 as aarch64_vmsa::regime::TranslationRegime>::Stage, G>,
{
    let mut segments = Vec::new();
    let mut virt_base = u64::MAX;
    let mut virt_end = 0u64;
    let mut load_count = 0usize;

    for ph in &kernel.program_headers {
        if ph.p_type != program_header::PT_LOAD {
            continue;
        }

        load_count += 1;

        if ph.p_memsz == 0 || ph.p_memsz < ph.p_filesz {
            return Err(KernelLoadError::InvalidSegmentSize);
        }

        let file_start = ph.p_offset as usize;
        let file_size = ph.p_filesz as usize;
        let file_end = file_start
            .checked_add(file_size)
            .ok_or(KernelLoadError::InvalidSegmentOffset)?;

        if file_end > kernel_bytes.len() {
            return Err(KernelLoadError::InvalidSegmentOffset);
        }

        let virt_start = ph.p_vaddr;
        let virt_page = align_down(virt_start, PAGE_SIZE as u64);
        let page_offset = virt_start - virt_page;
        let mapped_size = align_up(
            page_offset
                .checked_add(ph.p_memsz)
                .ok_or(KernelLoadError::InvalidSegmentRange)?,
            PAGE_SIZE as u64,
        )?;

        if !valid_va_range(virt_page, mapped_size, va_bits) {
            return Err(KernelLoadError::InvalidVirtualRange);
        }

        if mapped_range_overlaps(&segments, virt_page, mapped_size) {
            return Err(KernelLoadError::MappedSegmentOverlap);
        }

        let page_count = usize_from_u64(mapped_size / PAGE_SIZE as u64)?;
        let allocation =
            boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, page_count)
                .map_err(|err| KernelLoadError::PhysicalAllocationFailed(err.status()))?;

        let phys_page = allocation.as_ptr() as u64;
        let copy_dst = phys_page
            .checked_add(page_offset)
            .ok_or(KernelLoadError::InvalidSegmentRange)?;

        unsafe {
            ptr::write_bytes(phys_page as *mut u8, 0, usize_from_u64(mapped_size)?);
            ptr::copy_nonoverlapping(
                kernel_bytes[file_start..file_end].as_ptr(),
                copy_dst as *mut u8,
                file_size,
            );
        }

        let perms = segment_perms(ph.p_flags);

        mapper
            .map_kernel_range(config, virt_page, phys_page, mapped_size, perms)
            .map_err(KernelLoadError::MappingFailed)?;

        segments.push(LoadedSegment {
            virt_start: virt_page,
            phys_start: phys_page,
            byte_len: mapped_size,
            perms,
        });

        virt_base = cmp::min(virt_base, virt_page);
        virt_end = cmp::max(
            virt_end,
            virt_page
                .checked_add(mapped_size)
                .ok_or(KernelLoadError::InvalidSegmentRange)?,
        );
    }

    if load_count == 0 {
        return Err(KernelLoadError::MissingLoadSegment);
    }

    if !valid_va(kernel.entry, va_bits) {
        return Err(KernelLoadError::InvalidVirtualRange);
    }

    Ok(LoadedKernel {
        virt_entry: kernel.entry,
        virt_base,
        virt_size: virt_end - virt_base,
        segments,
    })
}

fn segment_perms(flags: u32) -> SegmentPerms {
    SegmentPerms {
        execute: (flags & 0x1) != 0,
        write: (flags & 0x2) != 0,
        read: (flags & 0x4) != 0,
    }
}

fn mapped_range_overlaps(segments: &[LoadedSegment], virt_start: u64, byte_len: u64) -> bool {
    let virt_end = match virt_start.checked_add(byte_len) {
        Some(end) => end,
        None => return true,
    };

    for segment in segments {
        let seg_end = match segment.virt_start.checked_add(segment.byte_len) {
            Some(end) => end,
            None => return true,
        };

        if virt_start < seg_end && segment.virt_start < virt_end {
            return true;
        }
    }

    false
}

fn valid_va_range(start: u64, byte_len: u64, va_bits: u32) -> bool {
    if byte_len == 0 {
        return false;
    }

    let end = match start.checked_add(byte_len - 1) {
        Some(end) => end,
        None => return false,
    };

    valid_va(start, va_bits) && valid_va(end, va_bits)
}

fn valid_va(addr: u64, va_bits: u32) -> bool {
    if va_bits == 0 || va_bits > 64 {
        return false;
    }

    if va_bits == 64 {
        return true;
    }

    let sign_bit = 1u64 << (va_bits - 1);
    let mask = !((1u64 << va_bits) - 1);

    if (addr & sign_bit) == 0 {
        (addr & mask) == 0
    } else {
        (addr & mask) == mask
    }
}

fn align_down(value: u64, align: u64) -> u64 {
    value & !(align - 1)
}

fn align_up<MapError>(value: u64, align: u64) -> Result<u64, KernelLoadError<MapError>> {
    let mask = align - 1;

    value
        .checked_add(mask)
        .map(|value| value & !mask)
        .ok_or(KernelLoadError::InvalidSegmentRange)
}

fn usize_from_u64<MapError>(value: u64) -> Result<usize, KernelLoadError<MapError>> {
    if value > usize::MAX as u64 {
        return Err(KernelLoadError::InvalidSegmentRange);
    }

    Ok(value as usize)
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KernelValidationError {
    NotAarch64,
    NotElf64,
    NotLittleEndian,
    UnsupportedElfType,
    MissingEntry,
    MissingLoadSegment,
    InvalidLoadSegmentSize,
    InvalidLoadSegmentAlign,
    LoadSegmentAddressMismatch,
    LoadSegmentOverlap,
    DynamicKernelUnsupported,
    InterpreterUnsupported,
}
pub fn validate_kernel(kernel: &Elf) -> Result<(), KernelValidationError> {
    if kernel.header.e_machine != header::EM_AARCH64 {
        return Err(KernelValidationError::NotAarch64);
    }

    if !kernel.is_64 {
        return Err(KernelValidationError::NotElf64);
    }

    if !kernel.little_endian {
        return Err(KernelValidationError::NotLittleEndian);
    }

    match kernel.header.e_type {
        header::ET_EXEC | header::ET_DYN => {}
        _ => return Err(KernelValidationError::UnsupportedElfType),
    }

    if kernel.entry == 0 {
        return Err(KernelValidationError::MissingEntry);
    }

    if kernel.interpreter.is_some() {
        return Err(KernelValidationError::InterpreterUnsupported);
    }

    if kernel.dynamic.is_some() {
        return Err(KernelValidationError::DynamicKernelUnsupported);
    }

    let mut load_count = 0usize;

    for ph in &kernel.program_headers {
        if ph.p_type != program_header::PT_LOAD {
            continue;
        }

        load_count += 1;

        if ph.p_memsz < ph.p_filesz {
            return Err(KernelValidationError::InvalidLoadSegmentSize);
        }

        if ph.p_align != 0 && !ph.p_align.is_power_of_two() {
            return Err(KernelValidationError::InvalidLoadSegmentAlign);
        }

        if ph.p_align != 0 && (ph.p_vaddr % ph.p_align) != (ph.p_offset % ph.p_align) {
            return Err(KernelValidationError::LoadSegmentAddressMismatch);
        }
    }

    if load_count == 0 {
        return Err(KernelValidationError::MissingLoadSegment);
    }

    let mut i = 0usize;

    while i < kernel.program_headers.len() {
        let a = &kernel.program_headers[i];

        if a.p_type != program_header::PT_LOAD {
            i += 1;
            continue;
        }

        let a_start = a.p_vaddr;
        let a_end = a_start
            .checked_add(a.p_memsz)
            .ok_or(KernelValidationError::LoadSegmentOverlap)?;

        let mut j = i + 1;

        while j < kernel.program_headers.len() {
            let b = &kernel.program_headers[j];

            if b.p_type == program_header::PT_LOAD {
                let b_start = b.p_vaddr;
                let b_end = b_start
                    .checked_add(b.p_memsz)
                    .ok_or(KernelValidationError::LoadSegmentOverlap)?;

                if a_start < b_end && b_start < a_end {
                    return Err(KernelValidationError::LoadSegmentOverlap);
                }
            }

            j += 1;
        }

        i += 1;
    }

    Ok(())
}
