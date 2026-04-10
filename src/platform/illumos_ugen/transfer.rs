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
    },
    ControlIn {
        buf: *mut u8,
        // total_len includes control data packet + space to read data
        total_len: u32
        // This is the length we want to read
        data_in_len: u32
    },
    BulkIn {
        buf: *mut u8,
        // Data length to read
        len: u32,
    },
    BulkOut {
        buf: *mut u8,
        // Data length to write
        len: u32,
    }
}

impl TransferData {
    /*
    pub(super) fn new(endpoint: u8) -> TransferData {
        let mut empty = ManuallyDrop::new(Vec::with_capacity(0));

        TransferData {
            buf: empty.as_mut_ptr(),
            request_len: 0,
            endpoint,
            status: None,
            capacity: 0,
            initialized_len: 0,
        }
    }
    */

    pub(super) fn new_control_out(data: ControlOut) -> TransferData {
        //const OUT_EP: u8 = 0x00;

        //let mut t = TransferData::new(OUT_EP);
        let mut buffer = Buffer::new(SETUP_PACKET_SIZE.checked_add(data.data.len()).unwrap());
        buffer.extend_from_slice(&data.setup_packet());
        buffer.extend_from_slice(data.data);
        let buf = ManuallyDrop::new(buffer);
        TransferData {
            transfer: Some(TransferType::ControlOut {
                buf: buf.ptr,
                total_len: buf.len
            })
            status: None
        }
        //t.set_buffer(buffer);
        //t
    }

    pub(super) fn new_control_in(data: ControlIn) -> TransferData {
        //const IN_EP: u8 = 0x80;

        //let mut t = TransferData::new(IN_EP);
        let mut buffer = Buffer::new(SETUP_PACKET_SIZE.checked_add(data.length as usize).unwrap());
        buffer.extend_from_slice(&data.setup_packet());
        let buf = ManuallyDrop::new(buffer);
        TransferData {
            transfer: Some(TransferType::ControlIn {
                buf: buf.ptr,
                total_len: buf.len,
                data_in_len: data.length,
            }),
            status: None,
        }
        //t.set_buffer(buffer);
        //t
    }

    pub(super) fn new_bulk_in(buf: Buffer) -> TransferData {
        let buf = ManuallyDrop::new(buffer);
        TransferData {
            transfer: Some(TransferType::BulkIn {
                buf: buf.ptr,
                len: buf.len,
            }),
            status: None,
        }

    }

    pub(super) fn new_bulk_out(buf: Buffer) -> TransferData {
        let buf = ManuallyDrop::new(buffer);
        TransferData {
            transfer: Some(TransferType::BulkIn {
                buf: buf.ptr,
                len: buf.len,
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

    pub fn control_in_data(&self) -> &[u8] {
        let Ok(len) = self.status.unwrap() else {
            panic!("argh argh");
        };
        unsafe { std::slice::from_raw_parts(self.buf.add(SETUP_PACKET_SIZE), len as usize) }
    }

    pub fn status(&self) -> Result<(), TransferError> {
        match self.status {
            // XXX
            None => Ok(()),
            Some(Ok(_)) => Ok(()),
            // XXX
            Some(Err(_)) => Err(TransferError::Fault),
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
        let capacity = mem::replace(&mut self.capacity, 0);
        let len = match Direction::from_address(self.endpoint) {
            Direction::Out => self.request_len,
            Direction::In => len as u32,
        };
        let requested_len = mem::replace(&mut self.request_len, 0);

        Completion {
            status,
            actual_len: len as usize,
            buffer: Buffer {
                ptr,
                len,
                requested_len,
                capacity,
                allocator: crate::transfer::Allocator::Default,
            },
        }
    }
}

impl Pending<TransferData> {

    pub(super) fn transfer(&mut self, fd: impl AsFd) {
        match self.transfer {
            Transfer::ControlOut { buf, total_len } => {
                // Rustix wants to work on the full buffer which is not what nusb expects
                let buf = unsafe {
                    std::slice::from_raw_parts(
                        (*self.as_ptr()).buf,
                        (*self.as_ptr()).total_len as usize,
                        )
                    };

                let status = io::write(&fd, buf);
             //   unsafe {
            //(*alias).status = Some(status);
        //}
                self.status = Some(status);

            }
            Transfer::ControlIn { buf, total_len, data_in_len } => {
                todo!()

            }
            Transfer::BulkIn { buf, len } => {
                todo!()
            }
            Transfer::BulkOut {  buf, out } => {
                todo!()
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
