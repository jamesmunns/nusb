use crate::platform::illumos_ugen::{errno_to_transfer_error, ugen_to_transfer_error};
use crate::transfer::internal::Pending;
use crate::transfer::{
    internal::notify_completion, Buffer, Completion, ControlIn, ControlOut, TransferError,
    SETUP_PACKET_SIZE,
};
use rustix::fd::{BorrowedFd, OwnedFd};
use rustix::io;
use rustix::io::Errno;
use std::mem::ManuallyDrop;

// We have two possible cases for transfer errors: the raw read/write
// failed OR the read/write succeded and the stat fd returned an error.
// In the future I would love to differentiate these further...
#[derive(Clone, Copy)]
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

#[allow(dead_code)]
pub struct TransferData {
    pub(crate) status: Option<Result<usize, UsbResult>>,
    transfer: Option<TransferType>,
    aiocb: *mut libc::aiocb,
    // XXX I don't love this
    raw_stat_fd: i32,
}

unsafe impl Send for TransferData {}
unsafe impl Sync for TransferData {}

enum TransferType {
    ControlOut {
        buf: *mut u8,
        // total_len includes control data packet + data to send
        total_len: u32,
        requested_len: u32,
        capacity: u32,
    },
    ControlIn {
        buf: *mut u8,
        // total_len includes control data packet + space to read data
        total_len: u32,
        // This is the length we want to read
        data_in_len: u32,
        requested_len: u32,
        capacity: u32,
    },
    BulkIn {
        buf: *mut u8,
        // Data length to read
        len: u32,
        requested_len: u32,
        capacity: u32,
    },
    BulkOut {
        buf: *mut u8,
        // Data length to write
        len: u32,
        requested_len: u32,
        capacity: u32,
    },
}

impl TransferType {
    fn control_in_data(&self, len: usize) -> &[u8] {
        match self {
            TransferType::ControlIn { buf, .. } => unsafe {
                std::slice::from_raw_parts(buf.add(SETUP_PACKET_SIZE), len)
            },
            _ => panic!("state machine error, this is not control in"),
        }
    }
}

impl TransferData {
    pub(super) fn new_control_out(data: ControlOut) -> TransferData {
        let mut buffer = Buffer::new(SETUP_PACKET_SIZE.checked_add(data.data.len()).unwrap());
        buffer.extend_from_slice(&data.setup_packet());
        buffer.extend_from_slice(data.data);
        let buf = ManuallyDrop::new(buffer);
        TransferData {
            transfer: Some(TransferType::ControlOut {
                buf: buf.ptr,
                total_len: buf.len,
                capacity: buf.capacity,
                requested_len: buf.len,
            }),
            status: None,
            aiocb: std::ptr::null_mut(),
            raw_stat_fd: -1,
        }
    }

    pub(super) fn new_control_in(data: ControlIn) -> TransferData {
        let mut buffer = Buffer::new(SETUP_PACKET_SIZE.checked_add(data.length as usize).unwrap());
        buffer.extend_from_slice(&data.setup_packet());
        let buf = ManuallyDrop::new(buffer);
        TransferData {
            transfer: Some(TransferType::ControlIn {
                buf: buf.ptr,
                total_len: buf.len,
                data_in_len: data.length as u32,
                capacity: buf.capacity,
                requested_len: buf.requested_len,
            }),
            status: None,
            aiocb: std::ptr::null_mut(),
            raw_stat_fd: -1,
        }
    }

    pub(super) fn new_bulk_in(buffer: Buffer) -> TransferData {
        let buf = ManuallyDrop::new(buffer);
        TransferData {
            transfer: Some(TransferType::BulkIn {
                buf: buf.ptr,
                len: buf.requested_len,
                capacity: buf.capacity,
                requested_len: buf.requested_len,
            }),
            status: None,
            aiocb: std::ptr::null_mut(),
            raw_stat_fd: -1,
        }
    }

    pub(super) fn new_bulk_out(buffer: Buffer) -> TransferData {
        let buf = ManuallyDrop::new(buffer);
        TransferData {
            transfer: Some(TransferType::BulkOut {
                buf: buf.ptr,
                len: buf.len,
                capacity: buf.capacity,
                requested_len: buf.len,
            }),
            status: None,
            aiocb: std::ptr::null_mut(),
            raw_stat_fd: -1,
        }
    }

    pub fn control_in_status(&self) -> Result<&[u8], TransferError> {
        match (&self.status, &self.transfer) {
            (None, _) | (_, None) => panic!("internal state machine error"),
            (Some(Ok(len)), Some(t)) => Ok(t.control_in_data(*len)),
            (Some(Err(e)), _) => Err(e.to_transfer_error()),
        }
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
        let transfer = self.transfer.take();

        match transfer {
            Some(TransferType::ControlOut {
                buf,
                total_len: _,
                requested_len,
                capacity,
            }) => Completion {
                status,
                actual_len: requested_len as usize,
                buffer: Buffer {
                    ptr: buf,
                    len: requested_len,
                    requested_len,
                    capacity,
                    allocator: crate::transfer::Allocator::Default,
                },
            },
            Some(TransferType::ControlIn {
                buf,
                total_len: _,
                data_in_len: _,
                requested_len,
                capacity,
            }) => Completion {
                status,
                actual_len: len,
                buffer: Buffer {
                    ptr: buf,
                    len: len as u32,
                    requested_len,
                    capacity,
                    allocator: crate::transfer::Allocator::Default,
                },
            },
            Some(TransferType::BulkIn {
                buf,
                len: _,
                requested_len,
                capacity,
            }) => Completion {
                status,
                actual_len: len,
                buffer: Buffer {
                    ptr: buf,
                    len: len as u32,
                    requested_len,
                    capacity,
                    allocator: crate::transfer::Allocator::Default,
                },
            },
            Some(TransferType::BulkOut {
                buf,
                len: _,
                requested_len,
                capacity,
            }) => Completion {
                status,
                actual_len: len,
                buffer: Buffer {
                    ptr: buf,
                    len: len as u32,
                    requested_len,
                    capacity,
                    allocator: crate::transfer::Allocator::Default,
                },
            },
            None => panic!("state machine error"),
        }
    }
}

enum InternalDir {
    In,
    Out,
}

extern "C" fn aio_callback(arg: libc::sigval) {
    let alias: *mut TransferData = arg.sival_ptr.cast();

    let aiocb = unsafe { (*alias).aiocb };

    let status = match unsafe { libc::aio_error(aiocb) } {
        // The handling here is a mess because if `aio_error` is 0 this should
        // always return something non-zero.
        0 => Ok(unsafe { libc::aio_return(aiocb).try_into().unwrap() }),
        // Once again, the ugen man page says to check this only if the return
        // is -1
        n => {
            if n == -1 {
                let mut stat: [u8; 4] = [0; 4];
                // we expect the stat fd to still be alive at this point
                match io::read(
                    unsafe { BorrowedFd::borrow_raw((*alias).raw_stat_fd) },
                    &mut stat,
                ) {
                    Ok(_) => Err(UsbResult::UgenStat(u32::from_le_bytes(stat))),
                    // Return this errno?
                    Err(errno) => Err(UsbResult::Errno(errno)),
                }
            } else {
                Err(UsbResult::Errno(Errno::from_raw_os_error(n)))
            }
        }
    };

    unsafe {
        (*alias).status = Some(status);
        notify_completion::<TransferData>(alias)
    }
}

unsafe fn do_aio_transfer(
    fd: i32,
    stat_fd: i32,
    alias: *mut TransferData,
    aio_buf: &mut [u8],
    aio_nbytes: libc::size_t,
    dir: InternalDir,
) {
    // `aiocb` has private padding fields so alloc is the easiest way
    // to do this
    let layout = std::alloc::Layout::new::<libc::aiocb>();
    let aiocb: *mut libc::aiocb = std::alloc::alloc(layout) as _;

    if aiocb.is_null() {
        panic!("failed to allocate");
    }

    (*alias).raw_stat_fd = stat_fd;
    (*alias).aiocb = aiocb;

    (*aiocb).aio_fildes = fd;
    (*aiocb).aio_buf = aio_buf.as_mut_ptr() as _;
    (*aiocb).aio_lio_opcode = match dir {
        InternalDir::In => libc::LIO_READ,
        InternalDir::Out => libc::LIO_WRITE,
    };
    (*aiocb).aio_nbytes = aio_nbytes;
    // We're always at offset 0
    (*aiocb).aio_offset = 0;
    // Default is fine
    (*aiocb).aio_reqprio = 0;
    (*aiocb).aio_sigevent.sigev_notify = libc::SIGEV_THREAD;
    // This is not used
    (*aiocb).aio_sigevent.sigev_signo = 0;
    (*aiocb).aio_sigevent.ss_sp = aio_callback as *mut libc::c_void;
    (*aiocb).aio_sigevent.sigev_value = libc::sigval {
        sival_ptr: alias as *mut libc::c_void,
    };
    (*aiocb).aio_sigevent.sigev_notify_attributes = std::ptr::null();

    let result = match dir {
        InternalDir::In => libc::aio_read(aiocb),
        InternalDir::Out => libc::aio_write(aiocb),
    };

    // aio failed, just notify the completion now
    if result < 0 {
        (*alias).status = Some(Err(UsbResult::Errno(Errno::from_raw_os_error(*unsafe {
            libc::___errno()
        }))));
        notify_completion::<TransferData>(alias)
    }
}

fn handle_errno_result(
    status: Result<usize, Errno>,
    stat_fd: &OwnedFd,
) -> Result<usize, UsbResult> {
    if let Err(errno) = status {
        // The exact wording is that if the return value is -1 we should check the
        // stat fd
        if errno.raw_os_error() == -1 {
            let mut stat: [u8; 4] = [0; 4];
            match io::read(stat_fd, &mut stat) {
                Ok(_) => Err(UsbResult::UgenStat(u32::from_le_bytes(stat))),
                // Return this errno?
                Err(errno) => Err(UsbResult::Errno(errno)),
            }
        } else {
            // Some other error dealing with the
            status.map_err(UsbResult::Errno)
        }
    } else {
        status.map_err(UsbResult::Errno)
    }
}

impl Pending<TransferData> {
    pub(super) fn cancel(&self) {
        let alias: *mut TransferData = unsafe { &mut (*self.as_ptr()) as *mut _ };

        let aiocb = unsafe { (*alias).aiocb };

        if aiocb.is_null() {
            return;
        }

        unsafe {
            libc::aio_cancel((*aiocb).aio_fildes, &mut (*aiocb));
        }
    }

    pub(super) fn raw_transfer(&self, raw_fd: i32, raw_stat_fd: i32) {
        // This is really ugly and I'd love another solution but it's the
        // style of the platform code...
        let alias: *mut TransferData = unsafe { &mut (*self.as_ptr()) as *mut _ };

        match unsafe { &(*alias).transfer } {
            Some(TransferType::BulkIn { buf, len, .. }) => {
                let buf = unsafe { std::slice::from_raw_parts_mut(*buf, *len as usize) };

                unsafe {
                    do_aio_transfer(
                        raw_fd,
                        raw_stat_fd,
                        alias,
                        buf,
                        *len as usize,
                        InternalDir::In,
                    );
                }
            }
            Some(TransferType::BulkOut { buf, len, .. }) => {
                let buf = unsafe { std::slice::from_raw_parts_mut(*buf, *len as usize) };

                unsafe {
                    do_aio_transfer(
                        raw_fd,
                        raw_stat_fd,
                        alias,
                        buf,
                        *len as usize,
                        InternalDir::Out,
                    );
                }
            }

            _ => panic!("ugh"),
        }
    }

    pub(super) fn transfer(&self, fd: &OwnedFd, stat_fd: &OwnedFd) {
        let alias: *mut TransferData = unsafe { &mut (*self.as_ptr()) as *mut _ };

        match unsafe { &(*alias).transfer } {
            Some(TransferType::ControlOut { buf, total_len, .. }) => {
                let buf = unsafe { std::slice::from_raw_parts(*buf, *total_len as usize) };

                let status = handle_errno_result(io::write(fd, buf), stat_fd);
                unsafe {
                    (*alias).status = Some(status);
                }
            }
            Some(TransferType::ControlIn {
                buf,
                total_len,
                data_in_len,
                ..
            }) => {
                let buffer = unsafe { std::slice::from_raw_parts(*buf, *total_len as usize) };

                let status = handle_errno_result(io::write(fd, buffer), stat_fd);
                if status.is_err() {
                    unsafe {
                        (*alias).status = Some(status);
                    }
                    return;
                }

                let buffer = unsafe {
                    std::slice::from_raw_parts_mut(
                        (*buf).add(SETUP_PACKET_SIZE),
                        *data_in_len as usize,
                    )
                };
                let status = handle_errno_result(io::read(fd, buffer), stat_fd);
                unsafe {
                    (*alias).status = Some(status);
                }
            }
            None | Some(_) => {
                panic!("state machine error");
            }
        }
    }
}

impl Drop for TransferData {
    fn drop(&mut self) {
        if !self.aiocb.is_null() {
            unsafe {
                std::alloc::dealloc(self.aiocb as _, std::alloc::Layout::new::<libc::aiocb>())
            };
        } //unsafe {
          //    drop(Vec::from_raw_parts(self.buf, 0, self.capacity as usize));
          //}
    }
}
