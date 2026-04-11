use crate::platform::illumos_ugen::errno_to_transfer_error;
use crate::transfer::internal::Pending;
use crate::transfer::{
    Buffer, Completion, ControlIn, ControlOut, Direction, TransferError, SETUP_PACKET_SIZE,
};
use rustix::fd::AsFd;
use rustix::io;
use rustix::io::Errno;
use std::mem;
use std::mem::ManuallyDrop;

#[allow(dead_code)]
pub struct TransferData {
    //pub(crate) buf: *mut u8,
    //pub(crate) endpoint: u8,
    pub(crate) status: Option<Result<usize, Errno>>,
    //pub(crate) request_len: u32,
    //capacity: u32,
    //initialized_len: u32,
    transfer: Option<TransferType>,
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
            TransferType::ControlIn { buf, .. } => {
                unsafe { std::slice::from_raw_parts(buf.add(SETUP_PACKET_SIZE), len as usize) }
            }
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
                data_in_len: data.length as  u32,
                capacity: buf.capacity,
                requested_len: buf.requested_len,
            }),
            status: None,
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
        }
    }

    //pub(super) fn set_buffer(&mut self, buf: Buffer) {
    //    debug_assert!(self.capacity == 0);
    //    let buf = ManuallyDrop::new(buf);
    //    self.capacity = buf.capacity;
    //    self.buf = buf.ptr;
    //    self.request_len = match Direction::from_address(self.endpoint) {
    //        Direction::Out => buf.len,
    //        Direction::In => buf.requested_len,
    //    };
    //    self.initialized_len = buf.len;
    //}

    pub fn control_in_status(&self) -> Result<&[u8], TransferError> {
        match (self.status, &self.transfer) {
            (None, _) | (_, None) => panic!("internal state machine error"),
            (Some(Ok(len)), Some(t)) => Ok(t.control_in_data(len)),
            (Some(Err(e)), _) => Err(errno_to_transfer_error(e)),
        }
    }

    pub fn status(&self) -> Result<(), TransferError> {
        match self.status {
            None => Ok(()),
            Some(Ok(_)) => Ok(()),
            Some(Err(e)) => Err(errno_to_transfer_error(e)),
        }
    }

    pub fn take_completion(&mut self) -> Completion {
        let (len, status) = match self.status.unwrap() {
            Ok(len) => (len, Ok(())),
            Err(err) => (0, Err(errno_to_transfer_error(err))),
        };

        self.status = None;
        let transfer = mem::replace(&mut self.transfer, None);

        //let mut empty = ManuallyDrop::new(Vec::new());
        //let ptr = mem::replace(&mut self.buf, empty.as_mut_ptr());
        //let capacity = mem::replace(&mut self.capacity, 0);
        //let len = match Direction::from_address(self.endpoint) {
        //    Direction::Out => self.request_len,
        //    Direction::In => len as u32,
        //};
        //let requested_len = mem::replace(&mut self.request_len, 0);
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
                actual_len: len as usize,
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
                actual_len: len as usize,
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
                actual_len: len as usize,
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

impl Pending<TransferData> {
    pub(super) fn transfer(&self, fd: impl AsFd) {
        // This is really ugly and I'd love another solution but it's the
        // style of the platform code...
        let alias: *mut TransferData = unsafe { &mut (*self.as_ptr()) as *mut _ };
        
        match unsafe { &(*alias).transfer} {
            Some(TransferType::ControlOut { buf, total_len, .. }) => {
                // Rustix wants to work on the full buffer which is not what nusb expects
                let buf = unsafe { std::slice::from_raw_parts(*buf, *total_len as usize) };

                let status = io::write(&fd, buf);
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
                // Rustix wants to work on the full buffer which is not what nusb expects
                let buffer = unsafe { std::slice::from_raw_parts(*buf, *total_len as usize) };

                let _ = io::write(&fd, buffer);

                let buffer = unsafe {
                    std::slice::from_raw_parts_mut((*buf).add(SETUP_PACKET_SIZE), *data_in_len as usize)
                };
                unsafe {
                    (*alias).status = Some(io::read(&fd, buffer));
                }
            }
            Some(TransferType::BulkIn { buf, len, .. }) => {
                let buf = unsafe { std::slice::from_raw_parts_mut(*buf, *len as usize) };
                
                unsafe {
                    (*alias).status = Some(io::read(&fd, buf));
                }
            }
            Some(TransferType::BulkOut { buf, len, .. }) => {
                let buf = unsafe { std::slice::from_raw_parts(*buf, *len as usize) };

                unsafe {
                    (*alias).status = Some(io::write(&fd, buf));
                }
            }
            None => {
                panic!("state machine error");
            }
        }
    }

    /*
    pub(super) fn ep_transfer(&self, fd: impl AsFd, dir: Direction) {
        let alias: *mut TransferData = unsafe { &mut (*self.as_ptr()) as *mut _ };

        let buf = unsafe {
            std::slice::from_raw_parts_mut(
                (*self.as_ptr()).buf,
                (*self.as_ptr()).request_len as usize,
            )
        };

        match dir {
            Direction::In => unsafe {
                (*alias).status = Some(io::read(&fd, buf));
            },
            Direction::Out => unsafe {
                (*alias).status = Some(io::write(&fd, buf));
            },
        }
    }

    pub(super) fn control_transfer(&self, fd: impl AsFd, dir: Direction) {
        let alias: *mut TransferData = unsafe { &mut (*self.as_ptr()) as *mut _ };

        // Rustix wants to work on the full buffer which is not what nusb expects
        let buf = unsafe {
            std::slice::from_raw_parts(
                (*self.as_ptr()).buf,
                (*self.as_ptr()).initialized_len as usize,
            )
        };

        let status = io::write(&fd, buf);
        unsafe {
            (*alias).status = Some(status);
        }

        if status.is_ok() {
            match dir {
                Direction::In => {
                    let buf = unsafe {
                        std::slice::from_raw_parts_mut(
                            (*self.as_ptr()).buf.add(SETUP_PACKET_SIZE),
                            (*self.as_ptr()).request_len as usize,
                        )
                    };
                    unsafe {
                        (*alias).status = Some(io::read(&fd, buf));
                    }
                }
                // Nothing else to do with out
                Direction::Out => {}
            }
        }
    }
    */
}

impl Drop for TransferData {
    fn drop(&mut self) {
        //unsafe {
        //    drop(Vec::from_raw_parts(self.buf, 0, self.capacity as usize));
        //}
    }
}
