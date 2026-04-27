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
    pub(crate) fn to_transfer_error(self) -> TransferError {
        match self {
            UsbResult::Errno(e) => errno_to_transfer_error(e),
            UsbResult::UgenStat(e) => ugen_to_transfer_error(e),
        }
    }
}

pub struct TransferData {
    pub(crate) status: Option<Result<usize, UsbResult>>,
    transfer: Option<TransferParts>,

    // TODO(AJM): We *might* want to make this a `Box<UnsafeCell<aiocb>>` instead
    // of a `Box<aiocb>`, as the latter enforces uniqueness, but we're really
    // explicitly aliasing access between the aio kernel actions and userspace.
    // I want to audit this a bit more, we might leave the ptr as-is, and slap
    // the unsafecell on access.
    aiocb: *mut libc::aiocb,
    // We need this for aio error handling via the raw fd
    raw_stat_fd: i32,
}

unsafe impl Send for TransferData {}
unsafe impl Sync for TransferData {}

pub struct BlockingTransferData {
    kind: BlockingTransferType,
    pub(super) buffer: Buffer,
}

struct TransferParts {
    kind: AioTransferType,
    buffer: ManuallyDrop<Buffer>,
}

enum AioTransferType {
    BulkIn,
    BulkOut,
}

enum BlockingTransferType {
    ControlOut,
    ControlIn {
        // This is the length we want to read
        data_in_len: u32,
    },
}

impl BlockingTransferData {
    pub(super) fn new_control_out(data: ControlOut) -> BlockingTransferData {
        let mut buffer = Buffer::new(SETUP_PACKET_SIZE.checked_add(data.data.len()).unwrap());
        buffer.extend_from_slice(&data.setup_packet());
        buffer.extend_from_slice(data.data);
        BlockingTransferData {
            kind: BlockingTransferType::ControlOut,
            buffer,
        }
    }

    pub(super) fn new_control_in(data: ControlIn) -> BlockingTransferData {
        let mut buffer = Buffer::new(SETUP_PACKET_SIZE.checked_add(data.length as usize).unwrap());
        buffer.extend_from_slice(&data.setup_packet());
        BlockingTransferData {
            kind: BlockingTransferType::ControlIn {
                data_in_len: data.length as u32,
            },
            buffer,
        }
    }

    pub(super) fn blocking_transfer(
        &mut self,
        fd: &OwnedFd,
        stat_fd: &OwnedFd,
    ) -> Result<usize, UsbResult> {
        let Self { kind, buffer } = self;
        let buflen = buffer.len as usize;

        match kind {
            BlockingTransferType::ControlOut => {
                // SAFETY: this is reconstructing the buffer because rustix takes a slice
                // (as opposed to most platform APIs which do *magic* on the pointer
                let buf = unsafe { std::slice::from_raw_parts(buffer.ptr, buflen) };

                handle_errno_result(io::write(fd, buf), stat_fd)
            }
            BlockingTransferType::ControlIn { data_in_len } => {
                let status = {
                    // SAFETY: this is reconstructing the buffer because rustix takes a slice
                    // (as opposed to most platform APIs which do *magic* on the pointer
                    let buf = unsafe { std::slice::from_raw_parts(buffer.ptr, buflen) };

                    handle_errno_result(io::write(fd, buf), stat_fd)
                };
                if status.is_err() {
                    return status;
                }
                let dil = *data_in_len as usize;
                assert!((SETUP_PACKET_SIZE + dil) <= buflen);

                // SAFETY: same logic applies, this is our buffer we have constructed

                let buffer = unsafe {
                    let start = buffer.ptr.add(SETUP_PACKET_SIZE);
                    std::slice::from_raw_parts_mut(start, dil)
                };
                handle_errno_result(io::read(fd, buffer), stat_fd)
            }
        }
    }
}

impl TransferData {
    pub(super) fn new_bulk_in(buffer: Buffer) -> TransferData {
        let buffer = ManuallyDrop::new(buffer);
        TransferData {
            transfer: Some(TransferParts {
                kind: AioTransferType::BulkIn,
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
                kind: AioTransferType::BulkOut,
                buffer,
            }),
            status: None,
            aiocb: std::ptr::null_mut(),
            raw_stat_fd: -1,
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
            AioTransferType::BulkIn => {
                let mut buffer = ManuallyDrop::into_inner(buffer);
                buffer.len = len as u32;
                Completion {
                    status,
                    actual_len: len,
                    buffer,
                }
            }
            AioTransferType::BulkOut => Completion {
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
            AioTransferType::BulkIn => (buffer.ptr, buffer.requested_len as usize, InternalDir::In),
            AioTransferType::BulkOut => (buffer.ptr, buffer.len as usize, InternalDir::Out),
        }
    };

    alias.raw_stat_fd = stat_fd;

    // Depending on our direction, get the opcode and relevant "start" function
    type StartFn = unsafe extern "C" fn(*mut libc::aiocb) -> libc::c_int;
    let (opcode, aio_start_func): (libc::c_int, StartFn) = match dir {
        InternalDir::In => (libc::LIO_READ, libc::aio_read),
        InternalDir::Out => (libc::LIO_WRITE, libc::aio_write),
    };

    // Create our aiocb structure.
    // The assumption is we will only use this transfer request once.
    //
    // Store the aiocb ptr prior to calling the read/write function, as the
    // request is enqueued/active immediately.
    //
    // TODO: `Box::leak(Box::new(UnsafeCell::new(...)))`?
    alias.aiocb = Box::leak(Box::new(libc::aiocb {
        aio_fildes: fd,
        aio_buf: ptr.cast::<libc::c_void>(),
        aio_nbytes: nbytes,
        // We're always at offset zero
        aio_offset: 0,
        // Default is fine
        aio_reqprio: 0,
        aio_sigevent: {
            // SAFETY: `sigevent` has private padding fields, however the type itself has
            // no particular runtime invariants, and we are about to initalize all
            // visible fields before use. As this is a C-oriented structure, zero-initialization
            // of all fields is appropriate here.
            let mut sig_e = unsafe { MaybeUninit::<libc::sigevent>::zeroed().assume_init() };

            sig_e.sigev_notify = libc::SIGEV_THREAD;
            // This is not used, here for clarity
            sig_e.sigev_signo = 0;
            sig_e.ss_sp = aio_callback as *mut libc::c_void;
            sig_e.sigev_value = libc::sigval {
                sival_ptr: alias as *mut TransferData as *mut libc::c_void,
            };
            sig_e.sigev_notify_attributes = std::ptr::null();
            sig_e
        },

        aio_lio_opcode: opcode,

        aio_resultp: libc::aio_result_t {
            aio_return: 0,
            aio_errno: 0,
        },
        aio_state: 0,
        aio__pad: [0; _],
    }));

    // Trigger the start of the read/write
    let result = aio_start_func(alias.aiocb);

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
