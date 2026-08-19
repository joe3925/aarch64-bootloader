use alloc::vec::Vec;
use uefi::CString16;
use uefi::fs::Error;
use uefi::fs::FileSystem;

pub fn read_file(path: &str, fs: &mut FileSystem) -> Result<Vec<u8>, Error> {
    let c_path: CString16 = CString16::try_from(path).unwrap();
    fs.read(c_path.as_ref())
}
