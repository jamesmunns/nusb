use crate::{DeviceInfo, Error, Speed};

use super::DevfsPath;
use crate::bitset::EndpointBitSet;
use crate::descriptors::{
    parse_concatenated_config_descriptors, ConfigurationDescriptor, DeviceDescriptor,
    EndpointDescriptor, TransferType,
};
use crate::maybe_future::{blocking::Blocking, MaybeFuture};
use crate::platform::illumos_ugen::transfer::UsbResult;
use crate::platform::illumos_ugen::Errno;
use crate::platform::TransferData;
use crate::transfer::{
    internal::{
        notify_completion, take_completed_from_queue, Idle, Notify, Pending, TransferFuture,
    },
    Buffer, Completion, ControlIn, ControlOut, ControlType, Direction, Recipient, TransferError,
};
use crate::ErrorKind;
use log::debug;
use rustix::fd::AsRawFd;
use rustix::fd::OwnedFd;
use rustix::fs::{Mode, OFlags};
use rustix::io;
use std::collections::{HashMap, VecDeque};
use std::num::NonZero;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::{Context, Poll};
use std::time::Duration;

//
// Useful USB constants. We use names that deliberately match those found in
// the header files found under usr/src/uts/common/sys/usb.
//
const USB_CFG_DESCR_SIZE: u16 = 9;

const USB_REQ_GET_DESCR: u8 = 0x06;
const USB_REQ_GET_CFG: u8 = 0x08;
const USB_EP_DIR_MASK: u8 = 0x80;

enum DescriptorType {
    Device,
    Configuration {
        index: u8,
    },
    #[allow(dead_code)]
    String {
        index: u8,
    },
}

impl DescriptorType {
    fn to_value(&self) -> u16 {
        let high_byte = match self {
            DescriptorType::Device => 1,
            DescriptorType::Configuration { .. } => 2,
            DescriptorType::String { .. } => 3,
        } as u16;

        let low_byte = match self {
            DescriptorType::Device => 0,
            DescriptorType::Configuration { index } => *index,
            DescriptorType::String { index } => *index,
        } as u16;

        high_byte << 8 | low_byte
    }
}

pub(crate) struct IllumosEndpoint {
    inner: Arc<EndpointInner>,

    pub(crate) max_packet_size: usize,

    /// A queue of pending transfers, expected to complete in order
    pending: VecDeque<Pending<super::TransferData>>,

    idle_transfer: Option<Idle<TransferData>>,
}

impl IllumosEndpoint {
    pub(crate) fn endpoint_address(&self) -> u8 {
        self.inner.raw.address
    }

    pub(crate) fn pending(&self) -> usize {
        self.pending.len()
    }

    pub(crate) fn cancel_all(&mut self) {
        // Cancel transfers in reverse order to ensure subsequent transfers
        // can't complete out of order while we're going through them.
        for transfer in self.pending.iter_mut().rev() {
            transfer.cancel();
        }
    }

    fn make_transfer(&mut self, buffer: Buffer) -> Idle<TransferData> {
        self.idle_transfer.take().unwrap_or_else(|| {
            Idle::new(
                self.inner.clone(),
                match Direction::from_address(self.inner.raw.address) {
                    Direction::In => super::TransferData::new_bulk_in(buffer),
                    Direction::Out => super::TransferData::new_bulk_out(buffer),
                },
            )
        })
    }

    pub(crate) fn submit_err(&mut self, buffer: Buffer, error: TransferError) {
        assert_eq!(error, TransferError::InvalidArgument);
        let mut t = self.make_transfer(buffer);
        t.status = Some(Err(UsbResult::Errno(Errno::INVAL)));
        self.pending.push_back(t.simulate_complete());
    }

    pub(crate) fn submit(&mut self, buffer: Buffer) {
        let t = self.make_transfer(buffer);
        let pending = t.pre_submit();

        pending.raw_transfer(self.inner.fd.as_raw_fd(), self.inner.stat_fd.as_raw_fd());

        self.pending.push_back(pending);
    }

    pub(crate) fn poll_next_complete(&mut self, cx: &mut Context) -> Poll<Completion> {
        self.inner.notify.subscribe(cx);
        if let Some(mut transfer) = take_completed_from_queue(&mut self.pending) {
            let completion = transfer.take_completion();
            self.idle_transfer = Some(transfer);
            Poll::Ready(completion)
        } else {
            Poll::Pending
        }
    }

    pub(crate) fn wait_next_complete(&mut self, timeout: Duration) -> Option<Completion> {
        self.inner.notify.wait_timeout(timeout, || {
            take_completed_from_queue(&mut self.pending).map(|mut transfer| {
                let completion = transfer.take_completion();
                self.idle_transfer = Some(transfer);
                completion
            })
        })
    }

    pub(crate) fn clear_halt(&self) -> impl MaybeFuture<Output = Result<(), Error>> {
        let inner = self.inner.clone();
        Blocking::new(move || {
            let endpoint = inner.raw.address;
            debug!("Clear halt, endpoint {endpoint:02x}");
            todo!();
        })
    }
}

impl Drop for IllumosEndpoint {
    fn drop(&mut self) {
        if !self.pending.is_empty() {
            debug!(
                "Dropping endpoint {:02x} with {} pending transfers",
                self.inner.raw.address,
                self.pending.len()
            );
            self.cancel_all();
        }
    }
}

struct EndpointInner {
    raw: RawEndpoint,
    notify: Notify,
    interface: Arc<IllumosInterface>,
    fd: Arc<OwnedFd>,
    stat_fd: Arc<OwnedFd>,
}

impl Drop for EndpointInner {
    fn drop(&mut self) {
        let mut state = self.interface.state.lock().unwrap();
        state.endpoints.clear(self.raw.address);
    }
}

impl AsRef<Notify> for EndpointInner {
    fn as_ref(&self) -> &Notify {
        &self.notify
    }
}

#[derive(Clone)]
struct RawEndpoint {
    interface_number: u8,
    address: u8,
    #[allow(dead_code)]
    transfer_type: TransferType,
    direction: Direction,
}

impl RawEndpoint {
    fn device_basename(&self) -> String {
        format!(
            "if{}{}{}",
            self.interface_number,
            match self.direction {
                Direction::In => "in",
                Direction::Out => "out",
            },
            self.address & !USB_EP_DIR_MASK,
        )
    }

    fn stat_basename(&self) -> String {
        format!("{}stat", self.device_basename())
    }

    fn open_flags(&self) -> OFlags {
        OFlags::CLOEXEC
            | match self.direction {
                Direction::In => OFlags::RDONLY,
                Direction::Out => OFlags::WRONLY,
            }
    }
}

//#[derive(Debug)]
pub(crate) struct IllumosDevice {
    fd: OwnedFd,
    stat_fd: OwnedFd,
    device_descriptor: Vec<u8>,
    config_descriptors: Vec<u8>,
    active_config: u8,
    paths: DevfsPath,
    interfaces: HashMap<u8, Vec<RawEndpoint>>,
}

pub(crate) fn get_raw_string(fd: &OwnedFd, index: u8) -> Result<Vec<u8>, Error> {
    let mut result = get_raw(
        fd,
        DescriptorType::String { index },
        crate::descriptors::language_id::US_ENGLISH,
    )?;

    result.truncate(result[0].into());
    Ok(result)
}

fn get_descriptor(fd: &OwnedFd, descriptor_type: DescriptorType) -> Result<Vec<u8>, Error> {
    get_raw(fd, descriptor_type, 0)
}

fn get_raw(fd: &OwnedFd, descriptor_type: DescriptorType, index: u16) -> Result<Vec<u8>, Error> {
    #[allow(non_snake_case)]
    let wValue: u16 = descriptor_type.to_value();

    let mut control = ControlIn {
        control_type: ControlType::Standard,
        recipient: Recipient::Device,
        request: USB_REQ_GET_DESCR,
        value: wValue,
        index,
        length: USB_CFG_DESCR_SIZE,
    };

    io::write(fd, control.setup_packet().as_slice()).unwrap();

    let mut buf = [0u8; USB_CFG_DESCR_SIZE as usize];
    io::read(fd, &mut buf).unwrap();

    let total = u16::from_le_bytes(buf[2..4].try_into().unwrap());
    control.length = total;
    control.index = index;

    io::write(fd, control.setup_packet().as_slice()).unwrap();

    let mut descriptors = vec![0u8; total as usize];
    io::read(fd, &mut descriptors).unwrap();

    Ok(descriptors)
}

fn get_configuration(fd: &OwnedFd) -> Result<u8, Error> {
    let control = ControlIn {
        control_type: ControlType::Standard,
        recipient: Recipient::Device,
        request: USB_REQ_GET_CFG,
        value: 0,
        index: 0,
        length: 1,
    };

    let mut buf = [0u8];

    // XXX
    io::write(fd, control.setup_packet().as_slice()).unwrap();
    io::read(fd, &mut buf).unwrap();

    Ok(buf[0])
}

impl IllumosDevice {
    pub(crate) fn from_device_info(
        d: &DeviceInfo,
    ) -> impl MaybeFuture<Output = Result<Arc<IllumosDevice>, Error>> {
        //
        // We are going to open our control FD, and ask for descriptor
        // information.  (We expect this information to match that that's
        // already in the devinfo tree as the `usb-raw-cfg-descriptors`
        // property, but we don't cache that in `DeviceInfo`.)
        //
        let dpath = d.path.clone();

        Blocking::new(move || {
            let path = Path::new(
                dpath
                    .device_paths
                    .get("cntrl0")
                    .ok_or(Error::new(ErrorKind::Other, "not ugen").log_debug())?,
            );

            let fd = rustix::fs::open(path, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
                .map_err(|e| {
                    match e {
                        Errno::NOENT => {
                            Error::new_os(ErrorKind::Disconnected, "device not found", e)
                        }
                        Errno::PERM => {
                            Error::new_os(ErrorKind::PermissionDenied, "permission denied", e)
                        }
                        e => Error::new_os(ErrorKind::Other, "failed to open device", e),
                    }
                    .log_debug()
                })?;

            let stat_path = Path::new(
                dpath
                    .device_paths
                    .get("cntrl0stat")
                    .ok_or(Error::new(ErrorKind::Other, "not ugen").log_debug())?,
            );

            let stat_fd =
                rustix::fs::open(stat_path, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
                    .map_err(|e| {
                        match e {
                            Errno::NOENT => {
                                Error::new_os(ErrorKind::Disconnected, "device not found", e)
                            }
                            Errno::PERM => {
                                Error::new_os(ErrorKind::PermissionDenied, "permission denied", e)
                            }
                            e => Error::new_os(ErrorKind::Other, "failed to open device", e),
                        }
                        .log_debug()
                    })?;

            let device_descriptor = get_descriptor(&fd, DescriptorType::Device)?;
            let active_config = get_configuration(&fd)?;

            #[rustfmt::skip]
            let config_descriptors = get_descriptor(
                &fd, DescriptorType::Configuration { index: 0 },
            )?;

            let c = ConfigurationDescriptor::new(&config_descriptors).unwrap();

            let interfaces = c
                .interfaces()
                .map(|i| {
                    let alt = i.first_alt_setting();
                    let interface_number = alt.interface_number();

                    (
                        interface_number,
                        alt.endpoints()
                            .map(|ep| RawEndpoint {
                                interface_number,
                                address: ep.address(),
                                direction: ep.direction(),
                                transfer_type: ep.transfer_type(),
                            })
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<HashMap<_, _>>();

            Ok(Arc::new(Self {
                fd,
                stat_fd,
                device_descriptor,
                config_descriptors,
                active_config,
                paths: dpath.clone(),
                interfaces,
            }))
        })
    }

    pub(crate) fn device_descriptor(&self) -> DeviceDescriptor {
        DeviceDescriptor::new(&self.device_descriptor).unwrap()
    }

    pub(crate) fn control_in(
        self: Arc<Self>,
        data: ControlIn,
        _timeout: Duration,
    ) -> impl MaybeFuture<Output = Result<Vec<u8>, TransferError>> {
        let t = TransferData::new_control_in(data);
        TransferFuture::new(t, |t| self.submit(t)).map(move |t| {
            drop(self);
            t.control_in_status().map(|m| m.to_owned())
        })
    }

    pub(crate) fn control_out(
        self: Arc<Self>,
        data: ControlOut,
        _timeout: Duration,
    ) -> impl MaybeFuture<Output = Result<(), TransferError>> {
        let t = TransferData::new_control_out(data);
        TransferFuture::new(t, |t| self.submit(t)).map(move |t| {
            drop(self);
            t.status()
        })
    }

    pub(crate) fn configuration_descriptors(
        &self,
    ) -> impl Iterator<Item = ConfigurationDescriptor<'_>> {
        parse_concatenated_config_descriptors(&self.config_descriptors)
    }

    pub(crate) fn active_configuration_value(&self) -> u8 {
        self.active_config
    }

    #[allow(unused)]
    pub(crate) fn set_configuration(
        &self,
        configuration: u8,
    ) -> impl MaybeFuture<Output = Result<(), Error>> {
        // It doesn't look like libusb does this either since the model
        // of how ugen works doesn't match this
        Blocking::new(move || todo!("Not supported"))
    }

    pub(crate) fn reset(&self) -> impl MaybeFuture<Output = Result<(), Error>> {
        // Another API that isn't as easily exposed via ugen
        Blocking::new(move || todo!("Not supported"))
    }

    pub(crate) fn claim_interface(
        self: Arc<Self>,
        interface_number: u8,
    ) -> impl MaybeFuture<Output = Result<Arc<IllumosInterface>, Error>> {
        Blocking::new(move || {
            let Some(eps) = self.interfaces.get(&interface_number) else {
                return Err(Error::new(ErrorKind::Other, "invalid interface number").log_error());
            };

            let mut fds = HashMap::new();

            for ep in eps {
                let devname = ep.device_basename();

                let Some(path) = self.paths.device_paths.get(&devname) else {
                    return Err(Error::new(ErrorKind::Other, "bad device").log_error());
                };

                let fd = rustix::fs::open(path, ep.open_flags(), Mode::empty()).map_err(|e| {
                    let e: Option<u32> = e.raw_os_error().try_into().ok();
                    let code: Option<NonZero<u32>> = match e {
                        None => None,
                        Some(v) => v.try_into().ok(),
                    };
                    Error {
                        kind: ErrorKind::Other,
                        message: "opening device fd failed",
                        code,
                    }
                })?;

                let statname = ep.stat_basename();
                let Some(path) = self.paths.device_paths.get(&statname) else {
                    return Err(Error::new(ErrorKind::Other, "bad device stat").log_error());
                };

                let stat_fd =
                    rustix::fs::open(path, ep.open_flags(), Mode::empty()).map_err(|e| {
                        let e: Option<u32> = e.raw_os_error().try_into().ok();
                        let code: Option<NonZero<u32>> = match e {
                            None => None,
                            Some(v) => v.try_into().ok(),
                        };

                        Error {
                            kind: ErrorKind::Other,
                            message: "opening device stat fd failed",
                            code,
                        }
                    })?;

                fds.insert(
                    ep.address,
                    IllumosUsbFds {
                        fd: Arc::new(fd),
                        stat_fd: Arc::new(stat_fd),
                    },
                );
            }

            Ok(Arc::new(IllumosInterface {
                interface_number,
                fds,
                device: self.clone(),
                state: Mutex::new(InterfaceState::default()),
            }))
        })
    }

    pub(crate) fn submit(&self, transfer: Idle<TransferData>) -> Pending<TransferData> {
        let pending = transfer.pre_submit();

        pending.transfer(&self.fd, &self.stat_fd);

        unsafe {
            notify_completion::<TransferData>(pending.as_ptr());
        }
        pending
    }

    #[allow(unused)]
    pub(crate) fn detach_and_claim_interface(
        self: &Arc<Self>,
        interface_number: u8,
    ) -> impl MaybeFuture<Output = Result<Arc<IllumosInterface>, Error>> {
        // We may eventually want to do something here to detach but this
        // is okay for now
        self.clone().claim_interface(interface_number)
    }

    pub(crate) fn speed(&self) -> Option<Speed> {
        None
    }
}

#[derive(Default)]
struct InterfaceState {
    alt_setting: u8,
    endpoints: EndpointBitSet,
}

unsafe impl Sync for IllumosInterface {}

struct IllumosUsbFds {
    fd: Arc<OwnedFd>,
    stat_fd: Arc<OwnedFd>,
}

pub(crate) struct IllumosInterface {
    pub(crate) interface_number: u8,
    pub(crate) device: Arc<IllumosDevice>,
    fds: HashMap<u8, IllumosUsbFds>,
    state: Mutex<InterfaceState>,
}

impl IllumosInterface {
    pub fn control_in(
        &self,
        data: ControlIn,
        timeout: Duration,
    ) -> impl MaybeFuture<Output = Result<Vec<u8>, TransferError>> {
        self.device.clone().control_in(data, timeout)
    }

    pub fn control_out(
        self: Arc<Self>,
        data: ControlOut,
        timeout: Duration,
    ) -> impl MaybeFuture<Output = Result<(), TransferError>> {
        self.device.clone().control_out(data, timeout)
    }

    pub fn set_alt_setting(
        self: Arc<Self>,
        _alt_setting: u8,
    ) -> impl MaybeFuture<Output = Result<(), Error>> {
        // doesn't work exactly the same here
        Blocking::new(move || todo!("Not implemented"))
    }

    pub fn get_alt_setting(&self) -> u8 {
        self.state.lock().unwrap().alt_setting
    }

    pub fn endpoint(
        self: &Arc<Self>,
        descriptor: EndpointDescriptor,
    ) -> Result<IllumosEndpoint, Error> {
        let address = descriptor.address();
        let ep_type = descriptor.transfer_type();
        let max_packet_size = descriptor.max_packet_size();

        let mut state = self.state.lock().unwrap();

        if state.endpoints.is_set(address) {
            return Err(Error::new(ErrorKind::Busy, "endpoint already in use").log_error());
        }
        // This should have fewer unwraps
        let raw = self
            .device
            .interfaces
            .get(&self.interface_number)
            .unwrap()
            .iter()
            .find(|x| x.address == address && x.transfer_type == ep_type)
            .unwrap();

        state.endpoints.set(address);
        let fds = self.fds.get(&address).unwrap();
        Ok(IllumosEndpoint {
            inner: Arc::new(EndpointInner {
                raw: raw.clone(),
                interface: self.clone(),
                fd: fds.fd.clone(),
                stat_fd: fds.stat_fd.clone(),
                notify: Notify::new(),
            }),
            max_packet_size,

            pending: VecDeque::new(),
            idle_transfer: None,
        })
    }
}

impl Drop for IllumosInterface {
    fn drop(&mut self) {
        //
        // Nothing for the moment -- but this will need to unregister our
        // FDs from our event port
        //
    }
}
