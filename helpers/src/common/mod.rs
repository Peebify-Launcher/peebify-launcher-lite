pub mod errbuf;

#[cfg(windows)]
pub mod cmdline;
#[cfg(windows)]
pub mod process;
#[cfg(windows)]
pub mod section;

pub fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(core::iter::once(0)).collect()
}
