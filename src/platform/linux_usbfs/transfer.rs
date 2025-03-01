use std::{
    ffi::c_void,
    mem::{self, ManuallyDrop},
    ptr::null_mut,
    slice,
    sync::Arc,
};

use libc::realloc;
use rustix::io::Errno;

use crate::transfer::{
    Completion, ControlIn, ControlOut, PlatformSubmit, PlatformTransfer, RequestBuffer,
    RequestIsochronousBuffer, ResponseBuffer, TransferError, TransferType, SETUP_PACKET_SIZE,
};

use super::{
    errno_to_transfer_error,
    usbfs::{
        IsoPacketDesc, Urb, USBDEVFS_URB_TYPE_BULK, USBDEVFS_URB_TYPE_CONTROL,
        USBDEVFS_URB_TYPE_INTERRUPT, USBDEVFS_URB_TYPE_ISO,
    },
};

/// Linux-specific transfer state.
///
/// This logically contains a `Vec` with urb.buffer and capacity.
/// It also owns the `urb` allocation itself, which is stored out-of-line
/// to avoid violating noalias when submitting the transfer while holding
/// `&mut TransferData`.
pub struct TransferData {
    urb: *mut Urb,
    capacity: usize,
    device: Arc<super::Device>,

    /// Not directly used, exists just to keep the interface from being released
    /// while active.
    _interface: Option<Arc<super::Interface>>,
}

unsafe impl Send for TransferData {}

impl TransferData {
    pub(super) fn new(
        device: Arc<super::Device>,
        interface: Option<Arc<super::Interface>>,
        endpoint: u8,
        ep_type: TransferType,
    ) -> TransferData {
        let ep_type = match ep_type {
            TransferType::Control => USBDEVFS_URB_TYPE_CONTROL,
            TransferType::Interrupt => USBDEVFS_URB_TYPE_INTERRUPT,
            TransferType::Bulk => USBDEVFS_URB_TYPE_BULK,
            TransferType::Isochronous => USBDEVFS_URB_TYPE_ISO,
        };

        TransferData {
            urb: Box::into_raw(Box::new(Urb {
                ep_type,
                endpoint,
                status: 0,
                flags: 0,
                buffer: null_mut(),
                buffer_length: 0,
                actual_length: 0,
                start_frame: 0,
                number_of_packets_or_stream_id: 0,
                error_count: 0,
                signr: 0,
                usercontext: null_mut(),
                iso_frame_desc: [],
            })),
            capacity: 0,
            device,
            _interface: interface,
        }
    }

    fn urb_mut(&mut self) -> &mut Urb {
        // SAFETY: if we have `&mut`, the transfer is not pending
        unsafe { &mut *self.urb }
    }

    fn urb_setup_iso_packet_descriptors(&mut self, number_of_packets: usize, requested: usize) {
        unsafe {
            self.urb = realloc(
                self.urb as *mut c_void,
                size_of::<Urb>() + size_of::<IsoPacketDesc>() * number_of_packets,
            ) as *mut Urb;

            let urb = &mut *self.urb;

            urb.number_of_packets_or_stream_id = number_of_packets as u32;

            for iso_frame_desc in
                slice::from_raw_parts_mut(urb.iso_frame_desc.as_mut_ptr(), number_of_packets)
            {
                assert!(requested <= u32::MAX as usize);
                iso_frame_desc.length = requested as u32;
                iso_frame_desc.actual_length = 0;
                iso_frame_desc.status = 0;
            }
        }
    }

    fn fill(&mut self, v: Vec<u8>, len: usize, user_data: *mut c_void) {
        let mut v = ManuallyDrop::new(v);
        let urb = self.urb_mut();
        urb.buffer = v.as_mut_ptr();
        urb.buffer_length = len.try_into().expect("buffer size should fit in i32");
        urb.usercontext = user_data;
        urb.actual_length = 0;
        self.capacity = v.capacity();
    }

    /// SAFETY: requires that the transfer has completed and `length` bytes are initialized
    unsafe fn take_buf(&mut self, length: usize) -> Vec<u8> {
        let urb = self.urb_mut();
        assert!(!urb.buffer.is_null());
        let ptr = mem::replace(&mut urb.buffer, null_mut());
        let capacity = mem::replace(&mut self.capacity, 0);
        assert!(length <= capacity);
        Vec::from_raw_parts(ptr, length, capacity)
    }
}

impl Drop for TransferData {
    fn drop(&mut self) {
        unsafe {
            if !self.urb_mut().buffer.is_null() {
                drop(Vec::from_raw_parts(self.urb_mut().buffer, 0, self.capacity));
            }
            drop(Box::from_raw(self.urb));
        }
    }
}

impl PlatformTransfer for TransferData {
    fn cancel(&self) {
        unsafe {
            self.device.cancel_urb(self.urb);
        }
    }
}

impl PlatformSubmit<Vec<u8>> for TransferData {
    unsafe fn submit(&mut self, data: Vec<u8>, user_data: *mut c_void) {
        let ep = self.urb_mut().endpoint;
        assert!(ep & 0x80 == 0);
        let len = data.len();
        self.fill(data, len, user_data);

        // SAFETY: we just properly filled the buffer and it is not already pending
        unsafe { self.device.submit_urb(self.urb) }
    }

    unsafe fn take_completed(&mut self) -> Completion<ResponseBuffer> {
        let status = urb_status(self.urb_mut());
        let len = self.urb_mut().actual_length as usize;

        // SAFETY: self is completed (precondition)
        let data = ResponseBuffer::from_vec(self.take_buf(0), len);
        Completion { data, status }
    }
}

impl PlatformSubmit<RequestBuffer> for TransferData {
    unsafe fn submit(&mut self, data: RequestBuffer, user_data: *mut c_void) {
        let ep = self.urb_mut().endpoint;
        let ty = self.urb_mut().ep_type;
        assert!(ep & 0x80 == 0x80);
        assert!(ty == USBDEVFS_URB_TYPE_BULK || ty == USBDEVFS_URB_TYPE_INTERRUPT);

        let (data, len) = data.into_vec();
        self.fill(data, len, user_data);

        // SAFETY: we just properly filled the buffer and it is not already pending
        unsafe { self.device.submit_urb(self.urb) }
    }

    unsafe fn take_completed(&mut self) -> Completion<Vec<u8>> {
        let status = urb_status(self.urb_mut());
        let len = self.urb_mut().actual_length as usize;

        // SAFETY: self is completed (precondition) and `actual_length` bytes were initialized.
        let data = unsafe { self.take_buf(len) };
        Completion { data, status }
    }
}

impl PlatformSubmit<RequestIsochronousBuffer> for TransferData {
    unsafe fn submit(&mut self, data: RequestIsochronousBuffer, user_data: *mut c_void) {
        let ep = self.urb_mut().endpoint;
        let ty = self.urb_mut().ep_type;
        assert!(ep & 0x80 == 0x80);
        assert!(ty == USBDEVFS_URB_TYPE_ISO);

        self.urb_setup_iso_packet_descriptors(data.number_of_packets, data.requested);

        let (data, len) = data.into_vec();
        self.fill(data, len, user_data);

        // SAFETY: we just properly filled the buffer and it is not already pending
        unsafe { self.device.submit_urb(self.urb) }
    }

    unsafe fn take_completed(&mut self) -> Completion<Vec<Vec<u8>>> {
        let status = urb_status(self.urb_mut());
        let len = self.urb_mut().buffer_length as usize;

        // SAFETY: self is completed (precondition) and `actual_length` bytes were initialized.
        let buffer = unsafe { self.take_buf(len) };
        let mut data_start = 0;
        let mut data = Vec::new();

        for iso_packet_descriptor in unsafe { self.urb_mut().iso_packet_descriptors() } {
            if iso_packet_descriptor.status == 0 {
                let range = data_start..data_start + iso_packet_descriptor.actual_length as usize;

                data.push(buffer[range].to_vec());
            }

            data_start += iso_packet_descriptor.length as usize;
        }

        Completion { data, status }
    }
}

impl PlatformSubmit<ControlIn> for TransferData {
    unsafe fn submit(&mut self, data: ControlIn, user_data: *mut c_void) {
        let buf_len = SETUP_PACKET_SIZE + data.length as usize;
        let mut buf = Vec::with_capacity(buf_len);
        buf.extend_from_slice(&data.setup_packet());
        self.fill(buf, buf_len, user_data);

        // SAFETY: we just properly filled the buffer and it is not already pending
        unsafe { self.device.submit_urb(self.urb) }
    }

    unsafe fn take_completed(&mut self) -> Completion<Vec<u8>> {
        let status = urb_status(self.urb_mut());
        let len = self.urb_mut().actual_length as usize;

        // SAFETY: transfer is completed (precondition) and `actual_length`
        // bytes were initialized with setup buf in front
        let mut data = unsafe { self.take_buf(SETUP_PACKET_SIZE + len) };
        data.splice(0..SETUP_PACKET_SIZE, []);
        Completion { data, status }
    }
}

impl PlatformSubmit<ControlOut<'_>> for TransferData {
    unsafe fn submit(&mut self, data: ControlOut, user_data: *mut c_void) {
        let buf_len = SETUP_PACKET_SIZE + data.data.len();
        let mut buf = Vec::with_capacity(buf_len);
        buf.extend_from_slice(
            &data
                .setup_packet()
                .expect("data length should fit in setup packet's u16"),
        );
        buf.extend_from_slice(data.data);
        self.fill(buf, buf_len, user_data);

        // SAFETY: we just properly filled the buffer and it is not already pending
        unsafe { self.device.submit_urb(self.urb) }
    }

    unsafe fn take_completed(&mut self) -> Completion<ResponseBuffer> {
        let status = urb_status(self.urb_mut());
        let len = self.urb_mut().actual_length as usize;
        let data = ResponseBuffer::from_vec(self.take_buf(0), len);
        Completion { data, status }
    }
}

fn urb_status(urb: &Urb) -> Result<(), TransferError> {
    if urb.status == 0 {
        return Ok(());
    }

    // It's sometimes positive, sometimes negative, but rustix panics if negative.
    Err(errno_to_transfer_error(Errno::from_raw_os_error(
        urb.status.abs(),
    )))
}
