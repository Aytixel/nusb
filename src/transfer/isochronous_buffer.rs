use std::fmt::Debug;
use std::mem::ManuallyDrop;

use super::TransferRequest;

/// A buffer for requesting an IN transfer.
///
/// A `RequestIsochronousBuffer` is passed when submitting an `IN` transfer to define the
/// requested length and provide a buffer to receive data into. The buffer is
/// returned in the [`Completion`][`crate::transfer::Completion`] as a `Vec<Vec<u8>>`
/// with the data read from the endpoint. The `Vec`'s allocation can turned back
/// into a `RequestIsochronousBuffer` to re-use it for another transfer.
///
/// You can think of a `RequestIsochronousBuffer` as a `Vec` of `Vec` with uninitialized contents.
pub struct RequestIsochronousBuffer {
    pub(crate) buf: *mut u8,
    pub(crate) capacity: usize,
    pub(crate) requested: usize,
    pub(crate) number_of_packets: usize,
}

impl RequestIsochronousBuffer {
    /// Create a `RequestIsochronousBuffer` of the specified size.
    pub fn new(len: usize, number_of_packets: usize) -> RequestIsochronousBuffer {
        let mut v = ManuallyDrop::new(Vec::with_capacity(len * number_of_packets));
        RequestIsochronousBuffer {
            buf: v.as_mut_ptr(),
            capacity: v.capacity(),
            requested: len,
            number_of_packets,
        }
    }

    pub(crate) fn into_vec(self) -> (Vec<u8>, usize) {
        let s = ManuallyDrop::new(self);
        let v = unsafe { Vec::from_raw_parts(s.buf, 0, s.capacity) };
        (v, s.requested * s.number_of_packets)
    }

    /// Create a `RequestIsochronousBuffer` by re-using the allocation of a `Vec`.
    pub fn reuse(v: Vec<u8>, len: usize, number_of_packets: usize) -> RequestIsochronousBuffer {
        let mut v = ManuallyDrop::new(v);
        v.clear();
        v.reserve_exact(len * number_of_packets);
        RequestIsochronousBuffer {
            buf: v.as_mut_ptr(),
            capacity: v.capacity(),
            requested: len,
            number_of_packets,
        }
    }
}

unsafe impl Send for RequestIsochronousBuffer {}
unsafe impl Sync for RequestIsochronousBuffer {}

impl Drop for RequestIsochronousBuffer {
    fn drop(&mut self) {
        unsafe { drop(Vec::from_raw_parts(self.buf, 0, self.capacity)) }
    }
}

impl Debug for RequestIsochronousBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestIsochronousBuffer")
            .field("requested", &self.requested)
            .finish_non_exhaustive()
    }
}

impl TransferRequest for RequestIsochronousBuffer {
    type Response = Vec<Vec<u8>>;
}
