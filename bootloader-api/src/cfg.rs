extern crate alloc;

use alloc::vec::Vec;
use core::str;

#[derive(Clone, Copy, Debug)]
pub struct CfgEntry<'a> {
    pub key: &'a str,
    pub value: &'a str,
}

#[derive(Clone, Debug)]
pub struct CfgFile<'a> {
    entries: Vec<CfgEntry<'a>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CfgError {
    InvalidUtf8,
    MissingEquals { line: usize },
    EmptyKey { line: usize },
}

pub trait FromCfg<'a>: Sized {
    type Error;

    fn from_cfg(cfg: &CfgFile<'a>) -> Result<Self, Self::Error>;
}

impl<'a> CfgFile<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, CfgError> {
        let text = str::from_utf8(bytes).map_err(|_| CfgError::InvalidUtf8)?;
        let mut entries = Vec::new();

        for (idx, raw_line) in text.lines().enumerate() {
            let line_no = idx + 1;
            let line = strip_comment(raw_line).trim();

            if line.is_empty() {
                continue;
            }

            entries.push(parse_entry(line, line_no)?);
        }

        Ok(Self { entries })
    }

    pub fn get(&self, key: &'a str) -> Option<&'a str> {
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.key == key)
            .map(|entry| entry.value)
    }

    pub fn require(&self, key: &'a str) -> Result<&'a str, CfgLookupError<'a>> {
        self.get(key)
            .ok_or(CfgLookupError::MissingRequiredKey { key })
    }

    pub fn get_all<'b>(&'b self, key: &'b str) -> impl Iterator<Item = &'a str> + 'b {
        self.entries
            .iter()
            .filter(move |entry| entry.key == key)
            .map(|entry| entry.value)
    }

    pub fn has(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn entries(&self) -> &[CfgEntry<'a>] {
        &self.entries
    }

    pub fn read<T>(&self) -> Result<T, T::Error>
    where
        T: FromCfg<'a>,
    {
        T::from_cfg(self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CfgLookupError<'a> {
    MissingRequiredKey { key: &'a str },
}

fn parse_entry<'a>(line: &'a str, line_no: usize) -> Result<CfgEntry<'a>, CfgError> {
    let eq = match line.find('=') {
        Some(eq) => eq,
        None => return Err(CfgError::MissingEquals { line: line_no }),
    };

    let key = line[..eq].trim();
    let value = line[eq + 1..].trim();

    if key.is_empty() {
        return Err(CfgError::EmptyKey { line: line_no });
    }

    Ok(CfgEntry { key, value })
}

fn strip_comment(line: &str) -> &str {
    match line.as_bytes().iter().position(|&b| b == b'#') {
        Some(pos) => &line[..pos],
        None => line,
    }
}
