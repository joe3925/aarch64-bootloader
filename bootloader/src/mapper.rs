extern crate alloc;

use alloc::vec::Vec;
use core::arch::asm;
use core::marker::PhantomData;
use core::ptr::NonNull;

use aarch64_cpu::asm::barrier::{ISH, ISHST, SY, dsb, isb};
use aarch64_cpu::registers::{MAIR_EL1, TCR_EL1, TCR2_EL1, TTBR1_EL1};
use aarch64_vmsa::address::{GranuleKind, Level, PhysAddr, TranslationGranule};
use aarch64_vmsa::arch::{Capability, VmsaFeatures};
use aarch64_vmsa::attrs::{
    AllocationHints, CachePolicy, Cacheability, DataAccess, DirtyBitManagement, DirtyControl,
    MemoryAttributes, MemoryTransience, SemanticLeafAttrs, SemanticTableAttrs,
    SemanticVmsa64Stage1LeafControls, SemanticVmsa64Stage1TableControls, Shareability,
    SoftwareMetadata, Stage1EffectivePermissions, Stage1MemoryConfig, Stage1PermissionConfig,
    TwoPrivilegeTablePermissionLimits,
};
use aarch64_vmsa::config::format::{Vmsa64, Vmsa64Lpa2, Vmsa128};
use aarch64_vmsa::config::regime::NonSecureEl1Stage1;
use aarch64_vmsa::descriptor::{DescriptorFormat, HasLayout, SupportsLiveDescriptorIo};
use aarch64_vmsa::mapper::{Live, Mapper, MapperInvalidation, Offline};
use aarch64_vmsa::table::{
    RootTable, RootTableGeometry, TableAccess, TableAccessLocation, TableAccessMut, TableAddr,
    TableAllocLayout, TableFrameProvider, TableGeometry, TableReclaim, TableShape,
    TranslationTable, TranslationTableMut,
};
use aarch64_vmsa::translation::WalkInputAddr;
use tock_registers::interfaces::{Readable, Writeable};
use uefi::boot::{self, AllocateType};
use uefi::mem::memory_map::MemoryType;

use crate::kernel_loading::SegmentPerms;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MapperError {
    NullTable,
    NoMemory,
    InvalidGeometry,
    UnsupportedPhysicalAddressWidth(u8),
    UnsupportedDescriptorFormat,
}

pub struct IdentityTableAccess<F: DescriptorFormat, G: TranslationGranule>(PhantomData<(F, G)>);

impl<F: DescriptorFormat, G: TranslationGranule> IdentityTableAccess<F, G> {
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

unsafe impl<F: DescriptorFormat, G: TranslationGranule> TableAccess<F, G>
    for IdentityTableAccess<F, G>
{
    type Error = MapperError;

    fn table_at<'a>(
        &'a self,
        location: TableAccessLocation<'a, F, G>,
    ) -> Result<TranslationTable<'a, F, G>, Self::Error> {
        let ptr =
            NonNull::new(location.addr().raw() as *mut F::Raw).ok_or(MapperError::NullTable)?;
        Ok(unsafe { TranslationTable::from_raw_parts(ptr, location.shape()) })
    }
}

unsafe impl<F: DescriptorFormat, G: TranslationGranule> TableAccessMut<F, G>
    for IdentityTableAccess<F, G>
{
    fn table_at_mut<'a>(
        &'a mut self,
        location: TableAccessLocation<'a, F, G>,
    ) -> Result<TranslationTableMut<'a, F, G>, Self::Error> {
        let ptr =
            NonNull::new(location.addr().raw() as *mut F::Raw).ok_or(MapperError::NullTable)?;
        Ok(unsafe { TranslationTableMut::from_raw_parts(ptr, location.shape()) })
    }
}

struct PoolChunk {
    base: u64,
    bytes: usize,
    used: usize,
}

pub struct UefiTablePool<G: TranslationGranule> {
    chunks: Vec<PoolChunk>,
    _granule: PhantomData<G>,
}

pub struct FixedTablePool<G: TranslationGranule> {
    chunk: PoolChunk,
    _granule: PhantomData<G>,
}

impl<G: TranslationGranule> FixedTablePool<G> {
    pub fn new(table_count: usize) -> Result<Self, MapperError> {
        let usable = (G::SIZE as usize)
            .checked_mul(table_count)
            .ok_or(MapperError::NoMemory)?;
        let allocation_bytes = usable
            .checked_add(G::SIZE as usize - 1)
            .ok_or(MapperError::NoMemory)?;
        let pages = allocation_bytes.div_ceil(boot::PAGE_SIZE);
        let allocation =
            boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, pages)
                .map_err(|_| MapperError::NoMemory)?;
        let allocation_base = allocation.as_ptr() as u64;
        let base = allocation_base
            .checked_add(G::SIZE - 1)
            .ok_or(MapperError::NoMemory)?
            & !(G::SIZE - 1);
        unsafe { core::ptr::write_bytes(base as *mut u8, 0, usable) };
        Ok(Self {
            chunk: PoolChunk {
                base,
                bytes: usable,
                used: 0,
            },
            _granule: PhantomData,
        })
    }
}

unsafe impl<G: TranslationGranule> TableFrameProvider<G> for FixedTablePool<G> {
    type Error = MapperError;

    fn allocate_zeroed_table(
        &mut self,
        layout: TableAllocLayout,
    ) -> Result<TableAddr<G>, Self::Error> {
        let addr = UefiTablePool::<G>::allocate_from_chunk(&mut self.chunk, layout)
            .ok_or(MapperError::NoMemory)?;
        TableAddr::new(addr).map_err(|_| MapperError::InvalidGeometry)
    }

    fn reclaim_table(&mut self, _reclaim: TableReclaim<G>) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl<G: TranslationGranule> UefiTablePool<G> {
    pub const fn new() -> Self {
        Self {
            chunks: Vec::new(),
            _granule: PhantomData,
        }
    }

    fn allocate_from_chunk(chunk: &mut PoolChunk, layout: TableAllocLayout) -> Option<u64> {
        let align = layout.align() as usize;
        let start = chunk.used.checked_add(align - 1)? & !(align - 1);
        let end = start.checked_add(layout.bytes() as usize)?;
        if end > chunk.bytes {
            return None;
        }
        chunk.used = end;
        Some(chunk.base + start as u64)
    }
}

unsafe impl<G: TranslationGranule> TableFrameProvider<G> for UefiTablePool<G> {
    type Error = MapperError;

    fn allocate_zeroed_table(
        &mut self,
        layout: TableAllocLayout,
    ) -> Result<TableAddr<G>, Self::Error> {
        if let Some(addr) = self
            .chunks
            .last_mut()
            .and_then(|c| Self::allocate_from_chunk(c, layout))
        {
            return TableAddr::new(addr).map_err(|_| MapperError::InvalidGeometry);
        }
        let wanted = core::cmp::max(G::SIZE as usize * 16, layout.bytes() as usize);
        let allocation_bytes = wanted
            .checked_add(G::SIZE as usize - 1)
            .ok_or(MapperError::NoMemory)?;
        let pages = allocation_bytes.div_ceil(boot::PAGE_SIZE);
        let allocation =
            boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, pages)
                .map_err(|_| MapperError::NoMemory)?;
        let allocation_base = allocation.as_ptr() as u64;
        let base = allocation_base
            .checked_add(G::SIZE - 1)
            .ok_or(MapperError::NoMemory)?
            & !(G::SIZE - 1);
        let skipped = (base - allocation_base) as usize;
        let mut chunk = PoolChunk {
            base,
            bytes: pages * boot::PAGE_SIZE - skipped,
            used: 0,
        };
        unsafe {
            core::ptr::write_bytes(chunk.base as *mut u8, 0, chunk.bytes);
        }
        let addr =
            Self::allocate_from_chunk(&mut chunk, layout).ok_or(MapperError::InvalidGeometry)?;
        self.chunks.push(chunk);
        TableAddr::new(addr).map_err(|_| MapperError::InvalidGeometry)
    }

    fn reclaim_table(&mut self, _reclaim: TableReclaim<G>) -> Result<(), Self::Error> {
        Ok(())
    }
}

pub struct MappingPrimitive<F, G, A, P, M>
where
    F: DescriptorFormat,
    G: TranslationGranule,
{
    inner: Mapper<F, NonSecureEl1Stage1, G, A, P, M>,
}

impl<F, G, A, P, M> core::ops::Deref for MappingPrimitive<F, G, A, P, M>
where
    F: DescriptorFormat,
    G: TranslationGranule,
{
    type Target = Mapper<F, NonSecureEl1Stage1, G, A, P, M>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<F, G, A, P, M> core::ops::DerefMut for MappingPrimitive<F, G, A, P, M>
where
    F: DescriptorFormat,
    G: TranslationGranule,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<F, G, A, P> MappingPrimitive<F, G, A, P, Offline>
where
    F: DescriptorFormat
        + HasLayout<<NonSecureEl1Stage1 as aarch64_vmsa::regime::TranslationRegime>::Stage, G>,
    G: TranslationGranule,
    A: TableAccessMut<F, G>,
    P: TableFrameProvider<G>,
{
    pub fn create_offline(
        root: RootTable<F, NonSecureEl1Stage1, G>,
        access: A,
        frames: P,
    ) -> Result<Self, aarch64_vmsa::mapper::MapperError<A::Error, P::Error>> {
        Mapper::new_offline(root, access, frames).map(|inner| Self { inner })
    }

    pub fn into_parts(self) -> (RootTable<F, NonSecureEl1Stage1, G>, A, P) {
        self.inner.into_parts()
    }
}

impl<F, G, A, P, I> MappingPrimitive<F, G, A, P, Live<I>>
where
    F: DescriptorFormat
        + SupportsLiveDescriptorIo
        + HasLayout<<NonSecureEl1Stage1 as aarch64_vmsa::regime::TranslationRegime>::Stage, G>,
    G: TranslationGranule,
    A: TableAccessMut<F, G>,
    P: TableFrameProvider<G>,
    I: MapperInvalidation<F, G>,
{
    pub fn create_online(
        root: RootTable<F, NonSecureEl1Stage1, G>,
        access: A,
        frames: P,
        invalidation: I,
    ) -> Result<Self, aarch64_vmsa::mapper::MapperError<A::Error, P::Error>> {
        Mapper::new_live(root, access, frames, invalidation).map(|inner| Self { inner })
    }
}

pub struct BootPlanner<F, G, C = BootConfig>
where
    F: DescriptorFormat,
    G: TranslationGranule,
{
    mapper: MappingPrimitive<F, G, IdentityTableAccess<F, G>, UefiTablePool<G>, Offline>,
    config: C,
}

mod install_format_private {
    pub trait Sealed {}
}

pub trait InstallableDescriptorFormat: DescriptorFormat + install_format_private::Sealed {
    const USES_D128: bool;
    fn configure_tcr(tcr: &mut u64);
}

impl install_format_private::Sealed for Vmsa64 {}
impl InstallableDescriptorFormat for Vmsa64 {
    const USES_D128: bool = false;
    fn configure_tcr(tcr: &mut u64) {
        *tcr &= !(1 << 59);
    }
}

impl install_format_private::Sealed for Vmsa64Lpa2 {}
impl InstallableDescriptorFormat for Vmsa64Lpa2 {
    const USES_D128: bool = false;
    fn configure_tcr(tcr: &mut u64) {
        *tcr |= 1 << 59;
    }
}

impl install_format_private::Sealed for Vmsa128 {}
impl InstallableDescriptorFormat for Vmsa128 {
    const USES_D128: bool = true;
    fn configure_tcr(tcr: &mut u64) {
        *tcr &= !(1 << 59);
    }
}

impl<F, G, C> BootPlanner<F, G, C>
where
    F: DescriptorFormat
        + HasLayout<<NonSecureEl1Stage1 as aarch64_vmsa::regime::TranslationRegime>::Stage, G>,
    G: TranslationGranule,
{
    pub fn new(config: C, input_addr_bits: u8, output_addr_bits: u8) -> Result<Self, MapperError> {
        let mut pool = UefiTablePool::<G>::new();
        let level = TableGeometry::<F, G>::root_level_for_addr_bits(input_addr_bits);
        let shape = TableShape::<F, G>::new(level, 1).map_err(|_| MapperError::InvalidGeometry)?;
        let addr = pool.allocate_zeroed_table(
            shape
                .alloc_layout()
                .map_err(|_| MapperError::InvalidGeometry)?,
        )?;
        let geometry = RootTableGeometry::<F, G>::new(addr, input_addr_bits, output_addr_bits)
            .map_err(|_| MapperError::InvalidGeometry)?;
        let root = RootTable::from_geometry(geometry);
        let mapper = MappingPrimitive::create_offline(root, IdentityTableAccess::new(), pool)
            .map_err(|_| MapperError::InvalidGeometry)?;
        Ok(Self { mapper, config })
    }

    pub const fn root(&self) -> RootTable<F, NonSecureEl1Stage1, G> {
        self.mapper.inner.root()
    }

    pub fn mapping_parts_mut(
        &mut self,
    ) -> (
        &mut MappingPrimitive<F, G, IdentityTableAccess<F, G>, UefiTablePool<G>, Offline>,
        &C,
    ) {
        (&mut self.mapper, &self.config)
    }

    pub unsafe fn install<A, P, I>(
        self,
        new_access: A,
        new_frames: P,
        invalidation: I,
    ) -> Result<MappingPrimitive<F, G, A, P, Live<I>>, MapperError>
    where
        F: SupportsLiveDescriptorIo + InstallableDescriptorFormat,
        A: TableAccessMut<F, G>,
        P: TableFrameProvider<G>,
        I: MapperInvalidation<F, G>,
        C: BootTranslationConfig,
    {
        let features = VmsaFeatures::current();
        if !features.verify(F::REQUIRED_FEATURES) {
            return Err(MapperError::UnsupportedDescriptorFormat);
        }
        let root = self.mapper.into_parts().0;
        let ips = match root.output_addr_bits() {
            32 => 0,
            36 => 1,
            40 => 2,
            42 => 3,
            44 => 4,
            48 => 5,
            52 => 6,
            bits => return Err(MapperError::UnsupportedPhysicalAddressWidth(bits)),
        };
        let tg1 = match G::KIND {
            GranuleKind::Size4KiB => 2,
            GranuleKind::Size16KiB => 1,
            GranuleKind::Size64KiB => 3,
        };
        let mut tcr = TCR_EL1.get();
        const TTBR1_MASK: u64 = (0x7 << 32)
            | (0x3 << 30)
            | (0x3 << 28)
            | (0x3 << 26)
            | (0x3 << 24)
            | (1 << 23)
            | (0x3f << 16);
        tcr = (tcr & !TTBR1_MASK)
            | ((ips as u64) << 32)
            | ((tg1 as u64) << 30)
            | ((self.config.table_walk_shareability() as u64) << 28)
            | ((self.config.table_walk_outer_cacheability() as u64) << 26)
            | ((self.config.table_walk_inner_cacheability() as u64) << 24)
            | ((64u64 - root.addr_bits() as u64) << 16);
        F::configure_tcr(&mut tcr);
        let tcr2 = features.status(Capability::D128).is_implemented().then(|| {
            let value = TCR2_EL1.get();
            if F::USES_D128 {
                value | (1 << 5)
            } else {
                value & !(1 << 5)
            }
        });
        dsb(ISHST);
        MAIR_EL1.set(self.config.mair());
        if let Some(tcr2) = tcr2 {
            TCR2_EL1.set(tcr2);
        }
        TCR_EL1.set(tcr);
        TTBR1_EL1.set_baddr(root.addr().raw());
        isb(SY);
        unsafe {
            asm!("tlbi vmalle1is", options(nostack, preserves_flags));
        }
        dsb(ISH);
        isb(SY);
        MappingPrimitive::create_online(root, new_access, new_frames, invalidation)
            .map_err(|_| MapperError::InvalidGeometry)
    }
}

pub struct BootMapperInvalidation;
unsafe impl<G: TranslationGranule> MapperInvalidation<Vmsa64, G> for BootMapperInvalidation {
    fn leaf_inserted(&mut self, _: TableAccessLocation<Vmsa64, G>, _: usize, _: u64, _: u64) {}
    fn leaf_removed(&mut self, _: TableAccessLocation<Vmsa64, G>, _: usize, _: u64) {}
    fn table_descriptor_inserted(
        &mut self,
        _: TableAccessLocation<Vmsa64, G>,
        _: usize,
        _: u64,
        _: u64,
    ) {
    }
    fn table_descriptor_removed(&mut self, _: TableAccessLocation<Vmsa64, G>, _: usize, _: u64) {}
    fn before_table_frame_reclaim(&mut self, _: TableAddr<G>, _: TableAllocLayout) {}
    fn synchronize(&mut self) {
        dsb(ISHST);
        unsafe {
            asm!("tlbi vmalle1is", options(nostack, preserves_flags));
        }
        dsb(ISH);
        isb(SY);
    }
}

pub trait BootTranslationConfig: Stage1MemoryConfig {
    fn table_walk_shareability(&self) -> u8;
    fn table_walk_outer_cacheability(&self) -> u8;
    fn table_walk_inner_cacheability(&self) -> u8;
}

pub struct BootConfig {
    pub mair: u64,
    pub table_walk_shareability: u8,
    pub table_walk_outer_cacheability: u8,
    pub table_walk_inner_cacheability: u8,
}
impl Default for BootConfig {
    fn default() -> Self {
        Self {
            mair: 0xff,
            table_walk_shareability: 0b11,
            table_walk_outer_cacheability: 0b01,
            table_walk_inner_cacheability: 0b01,
        }
    }
}
impl Stage1MemoryConfig for BootConfig {
    fn mair(&self) -> u64 {
        self.mair
    }
}
impl Stage1PermissionConfig for BootConfig {}
impl BootTranslationConfig for BootConfig {
    fn table_walk_shareability(&self) -> u8 {
        self.table_walk_shareability & 0b11
    }
    fn table_walk_outer_cacheability(&self) -> u8 {
        self.table_walk_outer_cacheability & 0b11
    }
    fn table_walk_inner_cacheability(&self) -> u8 {
        self.table_walk_inner_cacheability & 0b11
    }
}

impl<G, A, P> MappingPrimitive<Vmsa64, G, A, P, Offline>
where
    G: TranslationGranule,
    A: TableAccessMut<Vmsa64, G>,
    P: TableFrameProvider<G>,
    Vmsa64: HasLayout<<NonSecureEl1Stage1 as aarch64_vmsa::regime::TranslationRegime>::Stage, G>,
{
    pub fn install_recursive_mapping(&mut self, index: usize) -> Result<u64, MapperError> {
        let root = self.root();
        if index >= TableGeometry::<Vmsa64, G>::entries() {
            return Err(MapperError::InvalidGeometry);
        }
        let mut base = 0u64;
        let mut level = root.level();
        loop {
            base |= (index as u64) << TableGeometry::<Vmsa64, G>::level_shift(level);
            if level == Vmsa64::FINAL_LEVEL {
                break;
            }
            level = level.next();
        }
        let sign_bit = 1u64 << (root.addr_bits() - 1);
        if base & sign_bit != 0 {
            base |= !((1u64 << root.addr_bits()) - 1);
        }
        let root_entry = unsafe { (root.addr().raw() as *const u64).add(index).read_volatile() };
        if root_entry != Vmsa64::invalid() {
            return Err(MapperError::InvalidGeometry);
        }
        unsafe {
            (root.addr().raw() as *mut u64)
                .add(index)
                .write_volatile(root.addr().raw() | 0b11)
        };
        Ok(base)
    }

    pub fn map_kernel_range<C>(
        &mut self,
        config: &C,
        virt_start: u64,
        phys_start: u64,
        byte_len: u64,
        perms: SegmentPerms,
    ) -> Result<(), MapperError>
    where
        C: Stage1MemoryConfig + Stage1PermissionConfig,
    {
        assert_eq!(virt_start % G::SIZE, 0);
        assert_eq!(phys_start % G::SIZE, 0);
        assert_eq!(byte_len % G::SIZE, 0);
        let wb = Cacheability::Cacheable {
            policy: CachePolicy::WriteBack,
            transience: MemoryTransience::NonTransient,
            allocation: AllocationHints::ReadWriteAllocate,
        };
        let leaf = SemanticLeafAttrs::<Vmsa64, NonSecureEl1Stage1> {
            memory: MemoryAttributes::Normal {
                inner: wb,
                outer: wb,
            },
            permissions: Stage1EffectivePermissions {
                privileged_data: if perms.write {
                    DataAccess::ReadWrite
                } else {
                    DataAccess::ReadOnly
                },
                unprivileged_data: DataAccess::None,
                privileged_execute: perms.execute,
                unprivileged_execute: false,
                privileged_gcs: false,
                unprivileged_gcs: false,
            },
            pas: (),
            controls: SemanticVmsa64Stage1LeafControls {
                shareability: Shareability::InnerShareable,
                access_flag: true,
                global: true,
                dirty: DirtyControl::Direct(DirtyBitManagement::SoftwareManaged),
                contiguous: false,
                guarded: false,
                software: SoftwareMetadata::new(0),
            },
        };
        let table = SemanticTableAttrs::<Vmsa64, NonSecureEl1Stage1> {
            permission_limits: TwoPrivilegeTablePermissionLimits {
                privileged_data_limit: DataAccess::ReadWrite,
                unprivileged_data_limit: DataAccess::None,
                privileged_execute_limit: true,
                unprivileged_execute_limit: false,
            },
            pas: (),
            controls: SemanticVmsa64Stage1TableControls::default(),
        };
        let mut off = 0;
        while off < byte_len {
            self.inner
                .map_semantic_leaf(
                    config,
                    WalkInputAddr::new(virt_start + off),
                    PhysAddr(phys_start + off),
                    F_L3,
                    leaf,
                    table,
                )
                .map_err(|_| MapperError::InvalidGeometry)?;
            off += G::SIZE;
        }
        Ok(())
    }
}

const F_L3: Level = Level::L3;
