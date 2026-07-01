#![no_std]
#![no_main]

pub mod cfg;
use crate::cfg::CfgFile;
use crate::cfg::FromCfg;
#[derive(Clone, Copy, Debug)]
pub struct BootConfig<'a> {
    pub kernel: &'a str,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootConfigError {
    MissingKernel,
}

impl<'a> FromCfg<'a> for BootConfig<'a> {
    type Error = BootConfigError;

    fn from_cfg(cfg: &CfgFile<'a>) -> Result<Self, Self::Error> {
        let kernel = cfg.get("kernel").ok_or(BootConfigError::MissingKernel)?;

        Ok(Self { kernel })
    }
}
