use crate::platform::illumos_ugen::{errno_to_transfer_error, ugen_to_transfer_error};
use crate::transfer::internal::Idle;
use crate::transfer::internal::Pending;
use crate::transfer::{
    internal::notify_completion, Buffer, Completion, ControlIn, ControlOut, TransferError,
    SETUP_PACKET_SIZE,
};
use core::mem::MaybeUninit;
use rustix::fd::{BorrowedFd, OwnedFd};
use rustix::io;
use rustix::io::Errno;
use std::cell::UnsafeCell;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicPtr, Ordering};

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

/// TransferData is held in various nusb containers like `Idle<P>` and
/// `Pending<P>`, and is a little tricky to work with because it contains
/// data that is shared between "userspace" and the AIO callback machinery.
///
/// When you directly own a `TransferData`, you are free to assume it is
/// NOT aliased, however if you access it via the `ptr` methods of (TODO)
pub struct TransferData {
    status: UnsafeCell<Option<Result<usize, UsbResult>>>,
    transfer: Option<TransferParts>,

    // TODO(AJM): We *might* want to make this a `Box<UnsafeCell<aiocb>>` instead
    // of a `Box<aiocb>`, as the latter enforces uniqueness, but we're really
    // explicitly aliasing access between the aio kernel actions and userspace.
    // I want to audit this a bit more, we might leave the ptr as-is, and slap
    // the unsafecell on access.
    aiocb: AtomicPtr<libc::aiocb>,
    // We need this for aio error handling via the raw fd
    raw_fd: i32,
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
        match kind {
            BlockingTransferType::ControlOut => handle_errno_result(io::write(fd, buffer), stat_fd),
            BlockingTransferType::ControlIn { data_in_len } => {
                let status = handle_errno_result(io::write(fd, buffer), stat_fd);
                if status.is_err() {
                    return status;
                }
                let dil = *data_in_len as usize;
                handle_errno_result(
                    io::read(
                        fd,
                        &mut buffer[SETUP_PACKET_SIZE..(SETUP_PACKET_SIZE + dil)],
                    ),
                    stat_fd,
                )
            }
        }
    }
}

impl Idle<TransferData> {
    // pub(super) fn status(&self) -> &Option<Result<usize, UsbResult>> {
    //     todo!()
    // }

    pub(super) fn status_mut(&mut self) -> &mut Option<Result<usize, UsbResult>> {
        // SAFETY: In Idle, there is no aliasing, and it is acceptable to get a
        // mutable reference to the status field
        self.status.get_mut()
    }

    // TODO: This probably should be `take_completion(self)`, but that would
    // require `Idle::<P>::into_inner() -> P`. Our TransferData is relatively
    // cheap: it's really just the box we keep it in, so there's less incentive
    // to keep it, and this would *probably* let us simplify some of the `status`
    // and `transfer` invariants: they could maybe always be `Some` (and therefore not
    // an Option at all).
    pub fn take_completion(&mut self) -> Completion {
        let (len, status) = match self.status_mut().take().unwrap() {
            Ok(len) => (len, Ok(())),
            Err(err) => (0, Err(err.to_transfer_error())),
        };
        let TransferParts { kind, buffer } =
            self.transfer.take().expect("should have transfer here");

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

    pub(super) fn raw_transfer(mut self, fd: i32, stat_fd: i32) -> Pending<TransferData> {
        //
        // At the start, we are in the idle state, and have exclusive access.
        //
        let (aiocb_ptr, aio_start_func) = {
            let idle: &mut TransferData = &mut self;

            let (ptr, nbytes, dir) = {
                let xfer = idle.transfer.as_ref().unwrap();
                let TransferParts { kind, buffer } = xfer;
                match kind {
                    AioTransferType::BulkIn => {
                        (buffer.ptr, buffer.requested_len as usize, InternalDir::In)
                    }
                    AioTransferType::BulkOut => (buffer.ptr, buffer.len as usize, InternalDir::Out),
                }
            };

            idle.raw_stat_fd = stat_fd;
            idle.raw_fd = fd;

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
            let aiocb_ptr = Box::leak(Box::new(libc::aiocb {
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
                    let mut sig_e =
                        unsafe { MaybeUninit::<libc::sigevent>::zeroed().assume_init() };

                    sig_e.sigev_notify = libc::SIGEV_THREAD;
                    // This is not used, here for clarity
                    sig_e.sigev_signo = 0;
                    sig_e.ss_sp = aio_callback as *mut libc::c_void;
                    sig_e.sigev_value = libc::sigval {
                        // Note: Fill in later!
                        // sival_ptr: alias as *mut TransferData as *mut libc::c_void,
                        sival_ptr: std::ptr::null_mut(),
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
            (aiocb_ptr, aio_start_func)
        };

        // END OF IDLE PHASE!
        //
        // BEGIN PENDING PHASE!
        let pending = self.pre_submit();
        let platform_ptr = pending.as_ptr();

        // SAFETY: We have NOT started the aio transfer yet, we are allowed to poke shared things
        // despite being in the pending state.
        let result = unsafe {
            // We can modify the contents of the aiocb as we have not launched it yet
            (&raw mut aiocb_ptr.aio_sigevent.sigev_value.sival_ptr)
                .write(platform_ptr as *mut libc::c_void);
            // We can modify the contents of the platform ptr as we have not launched the AIO yet
            (*platform_ptr).aiocb.store(aiocb_ptr, Ordering::Release);
            // Here we go!
            aio_start_func(aiocb_ptr)
        };

        // aio failed, just notify the completion now
        // The status will be updated in the callback for other cases
        if result < 0 {
            // Safety: Although we are pending, the AIO callback failed, which means
            // there is no chance of aliasing. Okay to take exclusive access.
            unsafe {
                // Get error
                let mut res = Some(Err(UsbResult::Errno(Errno::from_raw_os_error(
                    *libc::___errno(),
                ))));

                let alias = &mut *platform_ptr;
                let cur_status = &mut *alias.status.get();
                core::mem::swap(&mut res, cur_status);

                // Notify that the transfer is now complete (by way of error)
                notify_completion::<TransferData>(platform_ptr);
            }
        }

        // Return our transfer which is now in the pending state
        pending
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
            status: UnsafeCell::new(None),
            aiocb: AtomicPtr::new(std::ptr::null_mut()),
            raw_stat_fd: -1,
            raw_fd: -1,
        }
    }

    pub(super) fn new_bulk_out(buffer: Buffer) -> TransferData {
        let buffer = ManuallyDrop::new(buffer);
        TransferData {
            transfer: Some(TransferParts {
                kind: AioTransferType::BulkOut,
                buffer,
            }),
            status: UnsafeCell::new(None),
            aiocb: AtomicPtr::new(std::ptr::null_mut()),
            raw_stat_fd: -1,
            raw_fd: -1,
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
    // SAFETY: aio callback will ONLY be called when Pending, and we can
    // always treat TransferData as shared while in the Pending state.
    let alias_ptr = arg.sival_ptr.cast::<TransferData>();
    let alias = unsafe { &*alias_ptr };

    // TODO: Check non-null?
    let aiocb_ptr = alias.aiocb.load(Ordering::Acquire);

    let status = match unsafe { libc::aio_error(aiocb_ptr) } {
        // The handling here is a mess because if `aio_error` is 0 this should
        // always return something non-zero. This means the unwrap should
        // be fine
        //
        // Once again, the ugen man page says to check this only if the return
        0 => Ok(unsafe { libc::aio_return(aiocb_ptr).try_into().unwrap() }),
        // is -1
        -1 => {
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
        }
        other => Err(UsbResult::Errno(Errno::from_raw_os_error(other))),
    };

    unsafe {
        // we are done with our callback let it be dropped and catch
        // bad usage. Drop self...
        drop(Box::from_raw(aiocb_ptr));

        // ...set the status, which we (the callback!) are allowed to do in Pending
        let mut status = Some(status);
        let cur_stat = &mut *alias.status.get();
        core::mem::swap(&mut status, cur_stat);

        // And mark the ptr as null to signal that the callback is complete
        alias.aiocb.store(std::ptr::null_mut(), Ordering::Release);

        notify_completion::<TransferData>(alias_ptr)
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
        // SAFETY: In the Pending state, TransferData is always treated as
        // aliased, therefore it is valid to take a shared reference.
        let alias: *const TransferData = self.as_ptr().cast_const();
        let alias: &TransferData = unsafe { &*alias };

        let aiocb = alias.aiocb.load(Ordering::Acquire);
        if aiocb.is_null() {
            return;
        }

        unsafe {
            libc::aio_cancel(alias.raw_fd, aiocb);
        }

        // TODO: do we need to clear aiocb here? Check cancel result? If aiocb
        // is NOT null, then are WE responsible for the drop?
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
