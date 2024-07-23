mod transfer;
use rustix::io::Errno;
pub(crate) use transfer::TransferData;
mod usbfs;

mod enumeration;
mod events;
pub use enumeration::{list_devices, SysfsPath};

mod device;
pub(crate) use device::LinuxDetachedInterface as DetachedInterface;
pub(crate) use device::LinuxDevice as Device;
pub(crate) use device::LinuxInterface as Interface;

use crate::transfer::TransferError;

fn errno_to_transfer_error(e: Errno) -> TransferError {
    match e {
        Errno::NODEV | Errno::SHUTDOWN => TransferError::Disconnected,
        Errno::PIPE => TransferError::Stall,
        Errno::NOENT | Errno::CONNRESET | Errno::TIMEDOUT => TransferError::Cancelled,
        Errno::PROTO | Errno::ILSEQ | Errno::OVERFLOW | Errno::COMM | Errno::TIME => {
            TransferError::Fault
        }
        _ => TransferError::Unknown,
    }
}
