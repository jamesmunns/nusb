use crate::platform::illumos_ugen::{errno_to_transfer_error, ugen_to_transfer_error};
use crate::transfer::internal::Pending;
use crate::transfer::{
    internal::notify_completion, Buffer, Completion, ControlIn, ControlOut, TransferError,
    SETUP_PACKET_SIZE,
};
use core::mem::MaybeUninit;
use rustix::fd::{BorrowedFd, OwnedFd};
use rustix::io;
use rustix::io::Errno;
use std::mem::ManuallyDrop;

// We have two possible cases for transfer errors: the raw read/write
// failed OR the read/write succeded and the stat fd returned an error.
// In the future I would love to differentiate these further...
#[derive(Debug, Clone, Copy)]
pub(crate) enum UsbResult {
    Errno(Errno),
    UgenStat(u32),
}

impl UsbResult {
    fn to_transfer_error(self) -> TransferError {
        match self {
            UsbResult::Errno(e) => errno_to_transfer_error(e),
            UsbResult::UgenStat(e) => ugen_to_transfer_error(e),
        }
    }
}

pub struct TransferData {
    pub(crate) status: Option<Result<usize, UsbResult>>,
    transfer: Option<TransferParts>,
    aiocb: *mut libc::aiocb,
    // We need this for aio error handling via the raw fd
    raw_stat_fd: i32,
}

unsafe impl Send for TransferData {}
unsafe impl Sync for TransferData {}

struct TransferParts {
    kind: TransferType,
    buffer: ManuallyDrop<Buffer>,
}

enum TransferType {
    ControlOut,
    ControlIn {
        // This is the length we want to read
        data_in_len: u32,
    },
    BulkIn,
    BulkOut,
}

// impl TransferType {
//     fn control_in_data(&self, len: usize) -> Result<&[u8], TransferError> {
//         let TransferType::ControlIn { .. } = self else {
//             panic!("state machine error, this is not control in");
//         };
//         // This case should be in practice impossible because `len` is the
//         // result that we read out. We could attempt to bubble up an error
//         // but `TransferError` doesn't have a way to do internal error at
//         // the moment
//         if len >= buffer.len() {
//             panic!("internal error {} is greater than {}", len, buffer.len());
//         }
//         // SAFETY this is what was construted and previously passed to the kernel
//         // to read out
//         Ok(unsafe { std::slice::from_raw_parts(buffer.ptr.add(SETUP_PACKET_SIZE), len) })
//     }
// }

impl TransferData {
    pub(super) fn new_control_out(data: ControlOut) -> TransferData {
        let mut buffer = Buffer::new(SETUP_PACKET_SIZE.checked_add(data.data.len()).unwrap());
        buffer.extend_from_slice(&data.setup_packet());
        buffer.extend_from_slice(data.data);
        let buffer = ManuallyDrop::new(buffer);
        TransferData {
            transfer: Some(TransferParts {
                kind: TransferType::ControlOut,
                buffer,
            }),
            status: None,
            aiocb: std::ptr::null_mut(),
            raw_stat_fd: -1,
        }
    }

    pub(super) fn new_control_in(data: ControlIn) -> TransferData {
        let mut buffer = Buffer::new(SETUP_PACKET_SIZE.checked_add(data.length as usize).unwrap());
        buffer.extend_from_slice(&data.setup_packet());
        let buffer = ManuallyDrop::new(buffer);
        TransferData {
            transfer: Some(TransferParts {
                kind: TransferType::ControlIn {
                    data_in_len: data.length as u32,
                },
                buffer,
            }),
            status: None,
            aiocb: std::ptr::null_mut(),
            raw_stat_fd: -1,
        }
    }

    pub(super) fn new_bulk_in(buffer: Buffer) -> TransferData {
        let buffer = ManuallyDrop::new(buffer);
        TransferData {
            transfer: Some(TransferParts {
                kind: TransferType::BulkIn,
                buffer,
            }),
            status: None,
            aiocb: std::ptr::null_mut(),
            raw_stat_fd: -1,
        }
    }

    pub(super) fn new_bulk_out(buffer: Buffer) -> TransferData {
        let buffer = ManuallyDrop::new(buffer);
        TransferData {
            transfer: Some(TransferParts {
                kind: TransferType::BulkOut,
                buffer,
            }),
            status: None,
            aiocb: std::ptr::null_mut(),
            raw_stat_fd: -1,
        }
    }

    pub fn control_in_status(&self) -> Result<&[u8], TransferError> {
        todo!("AJM")
        // match (&self.status, &self.transfer) {
        //     (None, _) | (_, None) => panic!("internal state machine error"),
        //     (Some(Ok(len)), Some(t)) => t.control_in_data(*len),
        //     (Some(Err(e)), _) => Err(e.to_transfer_error()),
        // }
    }

    pub fn status(&self) -> Result<(), TransferError> {
        match &self.status {
            None => Ok(()),
            Some(Ok(_)) => Ok(()),
            Some(Err(e)) => Err(e.to_transfer_error()),
        }
    }

    pub fn take_completion(&mut self) -> Completion {
        let (len, status) = match self.status.unwrap() {
            Ok(len) => (len, Ok(())),
            Err(err) => (0, Err(err.to_transfer_error())),
        };

        self.status = None;
        let transfer = self.transfer.take().expect("should have transfer here");
        let TransferParts { kind, buffer } = transfer;

        match kind {
            TransferType::ControlOut => Completion {
                status,
                actual_len: len as usize,
                buffer: ManuallyDrop::into_inner(buffer),
            },
            TransferType::ControlIn { data_in_len: _ } => Completion {
                status,
                actual_len: len,
                buffer: ManuallyDrop::into_inner(buffer),
            },
            TransferType::BulkIn => {
                let mut buffer = ManuallyDrop::into_inner(buffer);
                buffer.len = len as u32;
                Completion {
                    status,
                    actual_len: len,
                    buffer,
                }
            }
            TransferType::BulkOut => Completion {
                status,
                actual_len: len,
                buffer: ManuallyDrop::into_inner(buffer),
            },
        }
    }
}

#[derive(Debug)]
enum InternalDir {
    In,
    Out,
}

const USB_LC_STAT_UNSPECIFIED_ERR: u32 = 0xe;

extern "C" fn aio_callback(arg: libc::sigval) {
    // SAFETY We're back from the kernel and we have ownership again
    let alias: &mut TransferData = unsafe { &mut *arg.sival_ptr.cast() };

    let status = match unsafe { libc::aio_error(alias.aiocb) } {
        // The handling here is a mess because if `aio_error` is 0 this should
        // always return something non-zero. This means the unwrap should
        // be fine
        0 => Ok(unsafe { libc::aio_return(alias.aiocb).try_into().unwrap() }), // Once again, the ugen man page says to check this only if the return
        // is -1
        n => {
            if n == -1 {
                let mut stat: [u8; 4] = [0; 4];
                match io::read(
                    // SAFETY we expect the stat fd to still be alive at this point
                    // and we stored it explicitly
                    unsafe { BorrowedFd::borrow_raw(alias.raw_stat_fd) },
                    &mut stat,
                ) {
                    Ok(4) => Err(UsbResult::UgenStat(u32::from_le_bytes(stat))),
                    // man page example just returns the unspecified error
                    Ok(_) => Err(UsbResult::UgenStat(USB_LC_STAT_UNSPECIFIED_ERR)),
                    // Return this errno?
                    Err(errno) => Err(UsbResult::Errno(errno)),
                }
            } else {
                Err(UsbResult::Errno(Errno::from_raw_os_error(n)))
            }
        }
    };

    unsafe {
        // we are done with our callback let it be dropped and catch
        // bad usage
        drop(Box::from_raw(alias.aiocb));
        (*alias).aiocb = std::ptr::null_mut();
        (*alias).status = Some(status);
        notify_completion::<TransferData>(alias)
    }
}

unsafe fn do_aio_transfer(fd: i32, stat_fd: i32, alias: &mut TransferData) {
    let (ptr, nbytes, dir) = {
        let xfer = alias.transfer.as_ref().unwrap();
        let TransferParts { kind, buffer } = xfer;
        match kind {
            TransferType::BulkIn => (buffer.ptr, buffer.requested_len as usize, InternalDir::In),
            TransferType::BulkOut => (buffer.ptr, buffer.len as usize, InternalDir::Out),
            _ => panic!(),
        }
    };

    // `aiocb` has private padding fields so MaybeUninit::zeroed is the
    // cleanest way to model this structure.
    // in the future it would be nifty to just use
    // Box::<libc::aiocb>::new_zeroed()
    let mut aiocb = Box::new(MaybeUninit::<libc::aiocb>::zeroed());
    let aiocb_ptr = aiocb.as_mut_ptr();

    alias.raw_stat_fd = stat_fd;

    // https://doc.rust-lang.org/stable/core/mem/union.MaybeUninit.html#initializing-a-struct-field-by-field
    (&raw mut (*aiocb_ptr).aio_fildes).write(fd);
    (&raw mut (*aiocb_ptr).aio_buf).write(ptr.cast::<libc::c_void>());
    (&raw mut (*aiocb_ptr).aio_lio_opcode).write(match dir {
        InternalDir::In => libc::LIO_READ,
        InternalDir::Out => libc::LIO_WRITE,
    });
    (&raw mut (*aiocb_ptr).aio_nbytes).write(nbytes);
    // We're always at offset zero
    (&raw mut (*aiocb_ptr).aio_offset).write(0);
    // Default is fine
    (&raw mut (*aiocb_ptr).aio_reqprio).write(0);
    (&raw mut (*aiocb_ptr).aio_sigevent.sigev_notify).write(libc::SIGEV_THREAD);
    // This is not used, here for clarity
    (&raw mut (*aiocb_ptr).aio_sigevent.sigev_signo).write(0);
    (&raw mut (*aiocb_ptr).aio_sigevent.ss_sp).write(aio_callback as *mut libc::c_void);
    (&raw mut (*aiocb_ptr).aio_sigevent.sigev_value).write(libc::sigval {
        sival_ptr: alias as *mut TransferData as *mut libc::c_void,
    });
    (&raw mut (*aiocb_ptr).aio_sigevent.sigev_notify_attributes).write(std::ptr::null());

    // We are done. The assumption is we will only use this transfer request once
    let mut aiocb_init = aiocb.assume_init();

    let result = match dir {
        InternalDir::In => libc::aio_read(aiocb_init.as_mut()),
        InternalDir::Out => libc::aio_write(aiocb_init.as_mut()),
    };

    alias.aiocb = Box::leak(aiocb_init);
    // aio failed, just notify the completion now
    // The status will be updated in the callback for other cases
    if result < 0 {
        alias.status = Some(Err(UsbResult::Errno(Errno::from_raw_os_error(*unsafe {
            libc::___errno()
        }))));
        notify_completion::<TransferData>(alias)
    }
}

fn handle_errno_result(
    status: Result<usize, Errno>,
    stat_fd: &OwnedFd,
) -> Result<usize, UsbResult> {
    let Err(errno) = status else {
        return status.map_err(UsbResult::Errno);
    };
    // The exact wording is that if the return value is -1 we should check the
    // stat fd
    if errno.raw_os_error() == -1 {
        let mut stat: [u8; 4] = [0; 4];
        match io::read(stat_fd, &mut stat) {
            Ok(4) => Err(UsbResult::UgenStat(u32::from_le_bytes(stat))),
            // man page example just returns the unspecified error
            Ok(_) => Err(UsbResult::UgenStat(USB_LC_STAT_UNSPECIFIED_ERR)),
            Err(errno) => Err(UsbResult::Errno(errno)),
        }
    } else {
        // Some other error dealing with the reading/writing. Treat this
        // a a standard errno
        status.map_err(UsbResult::Errno)
    }
}

impl Pending<TransferData> {
    pub(super) fn cancel(&self) {
        // SAFETY this is slightly unsafe because a transfer may be in flight
        // but importantly we're not touching the data itself, only getting
        // the pointer
        let alias: &mut TransferData = unsafe { &mut *self.as_ptr() };

        let aiocb = alias.aiocb;

        if aiocb.is_null() {
            return;
        }

        unsafe {
            libc::aio_cancel((*aiocb).aio_fildes, aiocb);
        }
    }

    pub(super) fn raw_transfer(&self, raw_fd: i32, raw_stat_fd: i32) {
        // SAFETY We're taking full ownership of this to do the transfer
        let alias: &mut TransferData = unsafe { &mut *self.as_ptr() };

        unsafe {
            do_aio_transfer(raw_fd, raw_stat_fd, alias);
        }
    }

    pub(super) fn transfer(&self, fd: &OwnedFd, stat_fd: &OwnedFd) {
        // SAFETY We're taking full ownership of this to do the transfer
        let alias: &mut TransferData = unsafe { &mut *self.as_ptr() };

        let Some(TransferParts { kind, buffer }) = alias.transfer.as_mut() else {
            panic!("state machine error");
        };

        match kind {
            TransferType::ControlOut => {
                // SAFETY: this is reconstructing the buffer because rustix takes a slice
                // (as opposed to most platform APIs which do *magic* on the pointer
                let buf = unsafe { std::slice::from_raw_parts(buffer.ptr, buffer.len as usize) };

                let status = handle_errno_result(io::write(fd, buf), stat_fd);
                alias.status = Some(status);
            }
            TransferType::ControlIn { data_in_len } => {
                // SAFETY: this is reconstructing the buffer because rustix takes a slice
                // (as opposed to most platform APIs which do *magic* on the pointer
                let buf = unsafe { std::slice::from_raw_parts(buffer.ptr, buffer.len as usize) };

                let status = handle_errno_result(io::write(fd, buf), stat_fd);
                if status.is_err() {
                    alias.status = Some(status);
                    return;
                }

                // SAFETY: same logic applies, this is our buffer we have constructed
                let buffer = unsafe {
                    std::slice::from_raw_parts_mut(
                        (*buffer).ptr.add(SETUP_PACKET_SIZE),
                        *data_in_len as usize,
                    )
                };
                let status = handle_errno_result(io::read(fd, buffer), stat_fd);
                alias.status = Some(status);
            }
            _ => {
                panic!("state machine error");
            }
        }
    }
}

impl Drop for TransferData {
    fn drop(&mut self) {
        // self.aiocb should get automatically dropped

        // If the completion was called this will be `None`
        let Some(transfer) = self.transfer.take() else {
            return;
        };
        drop(ManuallyDrop::into_inner(transfer.buffer));
    }
}
