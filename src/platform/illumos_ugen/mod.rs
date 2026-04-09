mod transfer;
use crate::ErrorKind;
use rustix::io::Errno;
use std::num::NonZeroU32;
pub(crate) use transfer::TransferData;
mod enumeration;
pub use enumeration::{list_buses, list_devices, DevfsPath};

mod device;
pub(crate) use device::IllumosDevice as Device;
pub(crate) use device::IllumosEndpoint as Endpoint;
pub(crate) use device::IllumosInterface as Interface;

mod hotplug;
pub(crate) use hotplug::IllumosHotplugWatch as HotplugWatch;

use crate::transfer::TransferError;

pub type DeviceId = u64;

fn errno_to_transfer_error(e: Errno) -> TransferError {
    match e {
        Errno::NODEV | Errno::SHUTDOWN => TransferError::Disconnected,
        Errno::PIPE => TransferError::Stall,
        Errno::NOENT | Errno::CONNRESET | Errno::TIMEDOUT => TransferError::Cancelled,
        Errno::PROTO | Errno::ILSEQ | Errno::OVERFLOW | Errno::COMM | Errno::TIME => {
            TransferError::Fault
        }
        Errno::BADF => {
            println!("we have closed it somehow?");

            TransferError::Unknown(e.raw_os_error() as u32)
        }
        _ => TransferError::Unknown(e.raw_os_error() as u32),
    }
}

pub fn format_os_error_code(f: &mut std::fmt::Formatter<'_>, code: u32) -> std::fmt::Result {
    write!(f, "errno {}", code)
}

impl crate::error::Error {
    pub(crate) fn new_os(kind: ErrorKind, message: &'static str, code: Errno) -> Self {
        Self {
            kind,
            code: NonZeroU32::new(code.raw_os_error() as u32),
            message,
        }
    }
}
