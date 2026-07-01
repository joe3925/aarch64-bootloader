use crate::kernel_loading::SegmentPerms;
pub trait KernelMapper {
    type Error;

    fn map_kernel_range(
        &mut self,
        virt_start: u64,
        phys_start: u64,
        byte_len: u64,
        perms: SegmentPerms,
    ) -> Result<(), Self::Error>;
}
pub enum MapError {}

pub struct BootMapper {}
impl KernelMapper for BootMapper {
    type Error = MapError;
    fn map_kernel_range(
        &mut self,
        virt_start: u64,
        phys_start: u64,
        byte_len: u64,
        perms: SegmentPerms,
    ) -> Result<(), Self::Error> {
        return Ok(());
    }
}
