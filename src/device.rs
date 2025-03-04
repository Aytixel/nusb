use crate::{
    descriptors::{
        decode_string_descriptor, validate_string_descriptor, ActiveConfigurationError,
        ConfigurationDescriptor, DeviceDescriptor, InterfaceDescriptor, DESCRIPTOR_TYPE_STRING,
    },
    platform,
    transfer::{
        Control, ControlIn, ControlOut, Queue, RequestBuffer, RequestIsochronousBuffer,
        TransferError, TransferFuture, TransferType,
    },
    DeviceInfo, Error, MaybeFuture, Speed,
};
use log::error;
use std::{io::ErrorKind, num::NonZeroU8, sync::Arc, time::Duration};

/// An opened USB device.
///
/// Obtain a `Device` by calling [`DeviceInfo::open`]:
///
/// ```no_run
/// use nusb::{self, MaybeFuture};
/// let device_info = nusb::list_devices().wait().unwrap()
///     .find(|dev| dev.vendor_id() == 0xAAAA && dev.product_id() == 0xBBBB)
///     .expect("device not connected");
///
/// let device = device_info.open().wait().expect("failed to open device");
/// let interface = device.claim_interface(0);
/// ```
///
/// This type is reference-counted with an [`Arc`] internally, and can be cloned cheaply for
/// use in multiple places in your program. The device is closed when all clones and all
/// associated [`Interface`]s are dropped.
///
/// Use [`.claim_interface(i)`][`Device::claim_interface`] to open an interface to submit
/// transfers.
#[derive(Clone)]
pub struct Device {
    backend: Arc<crate::platform::Device>,
}

impl Device {
    pub(crate) fn wrap(backend: Arc<platform::Device>) -> Device {
        Device { backend }
    }

    pub(crate) fn open(
        d: &DeviceInfo,
    ) -> impl MaybeFuture<Output = Result<Device, std::io::Error>> {
        platform::Device::from_device_info(d).map(|d| d.map(Device::wrap))
    }

    /// Wraps a device that is already open.
    #[cfg(any(target_os = "android", target_os = "linux"))]
    pub fn from_fd(fd: std::os::fd::OwnedFd) -> impl MaybeFuture<Output = Result<Device, Error>> {
        platform::Device::from_fd(fd).map(|d| d.map(Device::wrap))
    }

    /// Open an interface of the device and claim it for exclusive use.
    pub fn claim_interface(
        &self,
        interface: u8,
    ) -> impl MaybeFuture<Output = Result<Interface, Error>> {
        self.backend
            .clone()
            .claim_interface(interface)
            .map(|i| i.map(Interface::wrap))
    }

    /// Detach kernel drivers and open an interface of the device and claim it for exclusive use.
    ///
    /// ### Platform notes
    /// This function can only detach kernel drivers on Linux. Calling on other platforms has
    /// the same effect as [`claim_interface`][`Device::claim_interface`].
    pub fn detach_and_claim_interface(
        &self,
        interface: u8,
    ) -> impl MaybeFuture<Output = Result<Interface, Error>> {
        self.backend
            .clone()
            .detach_and_claim_interface(interface)
            .map(|i| i.map(Interface::wrap))
    }

    /// Detach kernel drivers for the specified interface.
    ///
    /// ### Platform notes
    /// This function can only detach kernel drivers on Linux. Calling on other platforms has
    /// no effect.
    pub fn detach_kernel_driver(&self, interface: u8) -> Result<(), Error> {
        #[cfg(target_os = "linux")]
        self.backend.detach_kernel_driver(interface)?;
        let _ = interface;

        Ok(())
    }

    /// Attach kernel drivers for the specified interface.
    ///
    /// ### Platform notes
    /// This function can only attach kernel drivers on Linux. Calling on other platforms has
    /// no effect.
    pub fn attach_kernel_driver(&self, interface: u8) -> Result<(), Error> {
        #[cfg(target_os = "linux")]
        self.backend.attach_kernel_driver(interface)?;
        let _ = interface;

        Ok(())
    }

    /// Get the device descriptor.
    ///
    /// This returns cached data and does not perform IO.
    pub fn device_descriptor(&self) -> DeviceDescriptor {
        self.backend.device_descriptor()
    }

    /// Get device speed.
    pub fn speed(&self) -> Option<Speed> {
        self.backend.speed()
    }

    /// Get information about the active configuration.
    ///
    /// This returns cached data and does not perform IO. However, it can fail if the
    /// device is unconfigured, or if it can't find a configuration descriptor for
    /// the configuration reported as active by the OS.
    pub fn active_configuration(
        &self,
    ) -> Result<ConfigurationDescriptor, ActiveConfigurationError> {
        let active = self.backend.active_configuration_value();

        self.configurations()
            .find(|c| c.configuration_value() == active)
            .ok_or_else(|| ActiveConfigurationError {
                configuration_value: active,
            })
    }

    /// Get an iterator returning information about each configuration of the device.
    ///
    /// This returns cached data and does not perform IO.
    pub fn configurations(&self) -> impl Iterator<Item = ConfigurationDescriptor> {
        self.backend.configuration_descriptors()
    }

    /// Set the device configuration.
    ///
    /// The argument is the desired configuration's `bConfigurationValue`
    /// descriptor field from [`Configuration::configuration_value`] or `0` to
    /// unconfigure the device.
    ///
    /// ### Platform-specific notes
    /// * Not supported on Windows
    pub fn set_configuration(
        &self,
        configuration: u8,
    ) -> impl MaybeFuture<Output = Result<(), Error>> {
        self.backend.clone().set_configuration(configuration)
    }

    /// Request a descriptor from the device.
    ///
    /// The `language_id` should be `0` unless you are requesting a string descriptor.
    ///
    /// ### Platform-specific details
    ///
    /// * On Windows, the timeout argument is ignored, and an OS-defined timeout is used.
    /// * On Windows, this does not wake suspended devices. Reading their
    ///   descriptors will return an error.
    pub fn get_descriptor(
        &self,
        desc_type: u8,
        desc_index: u8,
        language_id: u16,
        timeout: Duration,
    ) -> Result<Vec<u8>, Error> {
        #[cfg(target_os = "windows")]
        {
            let _ = timeout;
            self.backend
                .get_descriptor(desc_type, desc_index, language_id)
        }

        #[cfg(not(target_os = "windows"))]
        {
            const STANDARD_REQUEST_GET_DESCRIPTOR: u8 = 0x06;
            use crate::transfer::{ControlType, Recipient};

            let mut buf = vec![0; 4096];
            let len = self.control_in_blocking(
                Control {
                    control_type: ControlType::Standard,
                    recipient: Recipient::Device,
                    request: STANDARD_REQUEST_GET_DESCRIPTOR,
                    value: ((desc_type as u16) << 8) | desc_index as u16,
                    index: language_id,
                },
                &mut buf,
                timeout,
            )?;

            buf.truncate(len);
            Ok(buf)
        }
    }

    /// Request the list of supported languages for string descriptors.
    ///
    /// ### Platform-specific details
    ///
    /// See notes on [`get_descriptor`][`Self::get_descriptor`].
    pub fn get_string_descriptor_supported_languages(
        &self,
        timeout: Duration,
    ) -> Result<impl Iterator<Item = u16>, Error> {
        let data = self.get_descriptor(DESCRIPTOR_TYPE_STRING, 0, 0, timeout)?;

        if !validate_string_descriptor(&data) {
            error!("String descriptor language list read {data:?}, not a valid string descriptor");
            return Err(Error::new(
                ErrorKind::InvalidData,
                "string descriptor data was invalid",
            ));
        }

        //TODO: Use array_chunks once stable
        let mut iter = data.into_iter().skip(2);
        Ok(std::iter::from_fn(move || {
            Some(u16::from_le_bytes([iter.next()?, iter.next()?]))
        }))
    }

    /// Request a string descriptor from the device.
    ///
    /// Almost all devices support only the language ID [`US_ENGLISH`][`crate::descriptors::language_id::US_ENGLISH`].
    ///
    /// Unpaired UTF-16 surrogates will be replaced with `�`, like [`String::from_utf16_lossy`].
    ///
    /// ### Platform-specific details
    ///
    /// See notes on [`get_descriptor`][`Self::get_descriptor`].
    pub fn get_string_descriptor(
        &self,
        desc_index: NonZeroU8,
        language_id: u16,
        timeout: Duration,
    ) -> Result<String, Error> {
        let data = self.get_descriptor(
            DESCRIPTOR_TYPE_STRING,
            desc_index.get(),
            language_id,
            timeout,
        )?;

        decode_string_descriptor(&data)
            .map_err(|_| Error::new(ErrorKind::InvalidData, "string descriptor data was invalid"))
    }

    /// Reset the device, forcing it to re-enumerate.
    ///
    /// This `Device` will no longer be usable, and you should drop it and call
    /// [`super::list_devices`] to find and re-open it again.
    ///
    /// ### Platform-specific notes
    /// * Not supported on Windows
    pub fn reset(&self) -> impl MaybeFuture<Output = Result<(), Error>> {
        self.backend.clone().reset()
    }

    /// Synchronously perform a single **IN (device-to-host)** transfer on the default **control** endpoint.
    ///
    /// ### Platform-specific notes
    ///
    /// * Not supported on Windows. You must [claim an interface][`Device::claim_interface`]
    ///   and use the interface handle to submit transfers.
    /// * On Linux, this takes a device-wide lock, so if you have multiple threads, you
    ///   are better off using the async methods.
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "android"))]
    pub fn control_in_blocking(
        &self,
        control: Control,
        data: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, TransferError> {
        self.backend.control_in_blocking(control, data, timeout)
    }

    /// Synchronously perform a single **OUT (host-to-device)** transfer on the default **control** endpoint.
    ///
    /// ### Platform-specific notes
    ///
    /// * Not supported on Windows. You must [claim an interface][`Device::claim_interface`]
    ///   and use the interface handle to submit transfers.
    /// * On Linux, this takes a device-wide lock, so if you have multiple threads, you
    ///   are better off using the async methods.
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "android"))]
    pub fn control_out_blocking(
        &self,
        control: Control,
        data: &[u8],
        timeout: Duration,
    ) -> Result<usize, TransferError> {
        self.backend.control_out_blocking(control, data, timeout)
    }

    /// Asynchronously submit a single **IN (device-to-host)** transfer on the default **control** endpoint.
    ///
    /// ### Example
    ///
    /// ```no_run
    /// use futures_lite::future::block_on;
    /// use nusb::transfer::{ ControlIn, ControlType, Recipient };
    /// # use nusb::MaybeFuture;
    /// # fn main() -> Result<(), std::io::Error> {
    /// # let di = nusb::list_devices().wait().unwrap().next().unwrap();
    /// # let device = di.open().wait().unwrap();
    ///
    /// let data: Vec<u8> = block_on(device.control_in(ControlIn {
    ///     control_type: ControlType::Vendor,
    ///     recipient: Recipient::Device,
    ///     request: 0x30,
    ///     value: 0x0,
    ///     index: 0x0,
    ///     length: 64,
    /// })).into_result()?;
    /// # Ok(()) }
    /// ```
    ///
    /// ### Platform-specific notes
    ///
    /// * Not supported on Windows. You must [claim an interface][`Device::claim_interface`]
    ///   and use the interface handle to submit transfers.
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "android"))]
    pub fn control_in(&self, data: ControlIn) -> TransferFuture<ControlIn> {
        let mut t = self.backend.make_control_transfer();
        t.submit::<ControlIn>(data);
        TransferFuture::new(t)
    }

    /// Submit a single **OUT (host-to-device)** transfer on the default **control** endpoint.
    ///
    /// ### Example
    ///
    /// ```no_run
    /// use futures_lite::future::block_on;
    /// use nusb::transfer::{ ControlOut, ControlType, Recipient };
    /// # use nusb::MaybeFuture;
    /// # fn main() -> Result<(), std::io::Error> {
    /// # let di = nusb::list_devices().wait().unwrap().next().unwrap();
    /// # let device = di.open().wait().unwrap();
    ///
    /// block_on(device.control_out(ControlOut {
    ///     control_type: ControlType::Vendor,
    ///     recipient: Recipient::Device,
    ///     request: 0x32,
    ///     value: 0x0,
    ///     index: 0x0,
    ///     data: &[0x01, 0x02, 0x03, 0x04],
    /// })).into_result()?;
    /// # Ok(()) }
    /// ```
    ///
    /// ### Platform-specific notes
    ///
    /// * Not supported on Windows. You must [claim an interface][`Device::claim_interface`]
    ///   and use the interface handle to submit transfers.
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "android"))]
    pub fn control_out(&self, data: ControlOut) -> TransferFuture<ControlOut> {
        let mut t = self.backend.make_control_transfer();
        t.submit::<ControlOut>(data);
        TransferFuture::new(t)
    }
}

/// An opened interface of a USB device.
///
/// Obtain an `Interface` with the [`Device::claim_interface`] method.
///
/// This type is reference-counted with an [`Arc`] internally, and can be cloned cheaply for
/// use in multiple places in your program. The interface is released when all clones, and all
/// associated [`TransferFuture`]s and [`Queue`]s are dropped.
#[derive(Clone)]
pub struct Interface {
    backend: Arc<platform::Interface>,
}

impl Interface {
    pub(crate) fn wrap(backend: Arc<platform::Interface>) -> Self {
        Interface { backend }
    }
    /// Select the alternate setting of this interface.
    ///
    /// An alternate setting is a mode of the interface that makes particular endpoints available
    /// and may enable or disable functionality of the device. The OS resets the device to the default
    /// alternate setting when the interface is released or the program exits.
    pub fn set_alt_setting(&self, alt_setting: u8) -> impl MaybeFuture<Output = Result<(), Error>> {
        self.backend.clone().set_alt_setting(alt_setting)
    }

    /// Get the current alternate setting of this interface.
    pub fn get_alt_setting(&self) -> u8 {
        self.backend.get_alt_setting()
    }

    /// Synchronously perform a single **IN (device-to-host)** transfer on the default **control** endpoint.
    ///
    /// ### Platform-specific notes
    ///
    /// * On Linux, this takes a device-wide lock, so if you have multiple
    ///   threads, you are better off using the async methods.
    /// * On Windows, if the `recipient` is `Interface`, the WinUSB driver sends
    ///   the interface number in the least significant byte of `index`,
    ///   overriding any value passed. A warning is logged if the passed `index`
    ///   least significant byte differs from the interface number, and this may
    ///   become an error in the future.
    pub fn control_in_blocking(
        &self,
        control: Control,
        data: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, TransferError> {
        self.backend.control_in_blocking(control, data, timeout)
    }

    /// Synchronously perform a single **OUT (host-to-device)** transfer on the default **control** endpoint.
    ///
    /// ### Platform-specific notes
    ///
    /// * On Linux, this takes a device-wide lock, so if you have multiple
    ///   threads, you are better off using the async methods.
    /// * On Windows, if the `recipient` is `Interface`, the WinUSB driver sends
    ///   the interface number in the least significant byte of `index`,
    ///   overriding any value passed. A warning is logged if the passed `index`
    ///   least significant byte differs from the interface number, and this may
    ///   become an error in the future.
    pub fn control_out_blocking(
        &self,
        control: Control,
        data: &[u8],
        timeout: Duration,
    ) -> Result<usize, TransferError> {
        self.backend.control_out_blocking(control, data, timeout)
    }

    /// Submit a single **IN (device-to-host)** transfer on the default **control** endpoint.
    ///
    /// ### Example
    ///
    /// ```no_run
    /// use futures_lite::future::block_on;
    /// use nusb::transfer::{ ControlIn, ControlType, Recipient };
    /// # use nusb::MaybeFuture;
    /// # fn main() -> Result<(), std::io::Error> {
    /// # let di = nusb::list_devices().wait().unwrap().next().unwrap();
    /// # let device = di.open().wait().unwrap();
    /// # let interface = device.claim_interface(0).wait().unwrap();
    ///
    /// let data: Vec<u8> = block_on(interface.control_in(ControlIn {
    ///     control_type: ControlType::Vendor,
    ///     recipient: Recipient::Device,
    ///     request: 0x30,
    ///     value: 0x0,
    ///     index: 0x0,
    ///     length: 64,
    /// })).into_result()?;
    /// # Ok(()) }
    /// ```
    ///
    /// ### Platform-specific notes
    /// * On Windows, if the `recipient` is `Interface`, the WinUSB driver sends
    ///   the interface number in the least significant byte of `index`,
    ///   overriding any value passed. A warning is logged if the passed `index`
    ///   least significant byte differs from the interface number, and this may
    ///   become an error in the future.
    pub fn control_in(&self, data: ControlIn) -> TransferFuture<ControlIn> {
        let mut t = self.backend.make_transfer(0, TransferType::Control);
        t.submit::<ControlIn>(data);
        TransferFuture::new(t)
    }

    /// Submit a single **OUT (host-to-device)** transfer on the default **control** endpoint.
    ///
    /// ### Example
    ///
    /// ```no_run
    /// use futures_lite::future::block_on;
    /// use nusb::transfer::{ ControlOut, ControlType, Recipient };
    /// # use nusb::MaybeFuture;
    /// # fn main() -> Result<(), std::io::Error> {
    /// # let di = nusb::list_devices().wait().unwrap().next().unwrap();
    /// # let device = di.open().wait().unwrap();
    /// # let interface = device.claim_interface(0).wait().unwrap();
    ///
    /// block_on(interface.control_out(ControlOut {
    ///     control_type: ControlType::Vendor,
    ///     recipient: Recipient::Device,
    ///     request: 0x32,
    ///     value: 0x0,
    ///     index: 0x0,
    ///     data: &[0x01, 0x02, 0x03, 0x04],
    /// })).into_result()?;
    /// # Ok(()) }
    /// ```
    ///
    /// ### Platform-specific notes
    /// * On Windows, if the `recipient` is `Interface`, the WinUSB driver sends
    ///   the interface number in the least significant byte of `index`,
    ///   overriding any value passed. A warning is logged if the passed `index`
    ///   least significant byte differs from the interface number, and this may
    ///   become an error in the future.
    pub fn control_out(&self, data: ControlOut) -> TransferFuture<ControlOut> {
        let mut t = self.backend.make_transfer(0, TransferType::Control);
        t.submit::<ControlOut>(data);
        TransferFuture::new(t)
    }

    /// Submit a single **IN (device-to-host)** transfer on the specified **bulk** endpoint.
    ///
    /// * The requested length must be a multiple of the endpoint's maximum packet size
    /// * An IN endpoint address must have the top (`0x80`) bit set.
    pub fn bulk_in(&self, endpoint: u8, buf: RequestBuffer) -> TransferFuture<RequestBuffer> {
        let mut t = self.backend.make_transfer(endpoint, TransferType::Bulk);
        t.submit(buf);
        TransferFuture::new(t)
    }

    /// Submit a single **OUT (host-to-device)** transfer on the specified **bulk** endpoint.
    ///
    /// * An OUT endpoint address must have the top (`0x80`) bit clear.
    pub fn bulk_out(&self, endpoint: u8, buf: Vec<u8>) -> TransferFuture<Vec<u8>> {
        let mut t = self.backend.make_transfer(endpoint, TransferType::Bulk);
        t.submit(buf);
        TransferFuture::new(t)
    }

    /// Create a queue for managing multiple **IN (device-to-host)** transfers on a **bulk** endpoint.
    ///
    /// * An IN endpoint address must have the top (`0x80`) bit set.
    pub fn bulk_in_queue(&self, endpoint: u8) -> Queue<RequestBuffer> {
        Queue::new(self.backend.clone(), endpoint, TransferType::Bulk)
    }

    /// Create a queue for managing multiple **OUT (host-to-device)** transfers on a **bulk** endpoint.
    ///
    /// * An OUT endpoint address must have the top (`0x80`) bit clear.
    pub fn bulk_out_queue(&self, endpoint: u8) -> Queue<Vec<u8>> {
        Queue::new(self.backend.clone(), endpoint, TransferType::Bulk)
    }

    /// Submit a single **IN (device-to-host)** transfer on the specified **isochronous** endpoint.
    ///
    /// * The requested length must be a multiple of the endpoint's maximum packet size
    /// * An IN endpoint address must have the top (`0x80`) bit set.
    pub fn isochronous_in(
        &self,
        endpoint: u8,
        buf: RequestIsochronousBuffer,
    ) -> TransferFuture<RequestIsochronousBuffer> {
        let mut t = self
            .backend
            .make_transfer(endpoint, TransferType::Isochronous);
        t.submit(buf);
        TransferFuture::new(t)
    }

    /// Create a queue for managing multiple **IN (device-to-host)** transfers on a **isochronous** endpoint.
    ///
    /// * An IN endpoint address must have the top (`0x80`) bit set.
    pub fn isochronous_in_queue(&self, endpoint: u8) -> Queue<RequestIsochronousBuffer> {
        Queue::new(self.backend.clone(), endpoint, TransferType::Isochronous)
    }

    /// Submit a single **IN (device-to-host)** transfer on the specified **interrupt** endpoint.
    ///
    /// * The requested length must be a multiple of the endpoint's maximum packet size
    /// * An IN endpoint address must have the top (`0x80`) bit set.
    pub fn interrupt_in(&self, endpoint: u8, buf: RequestBuffer) -> TransferFuture<RequestBuffer> {
        let mut t = self
            .backend
            .make_transfer(endpoint, TransferType::Interrupt);
        t.submit(buf);
        TransferFuture::new(t)
    }

    /// Submit a single **OUT (host-to-device)** transfer on the specified **interrupt** endpoint.
    ///
    /// * An OUT endpoint address must have the top (`0x80`) bit clear.
    pub fn interrupt_out(&self, endpoint: u8, buf: Vec<u8>) -> TransferFuture<Vec<u8>> {
        let mut t = self
            .backend
            .make_transfer(endpoint, TransferType::Interrupt);
        t.submit(buf);
        TransferFuture::new(t)
    }

    /// Create a queue for managing multiple **IN (device-to-host)** transfers on an **interrupt** endpoint.
    ///
    /// * An IN endpoint address must have the top (`0x80`) bit set.
    pub fn interrupt_in_queue(&self, endpoint: u8) -> Queue<RequestBuffer> {
        Queue::new(self.backend.clone(), endpoint, TransferType::Interrupt)
    }

    /// Create a queue for managing multiple **OUT (device-to-host)** transfers on an **interrupt** endpoint.
    ///
    /// * An OUT endpoint address must have the top (`0x80`) bit clear.
    pub fn interrupt_out_queue(&self, endpoint: u8) -> Queue<Vec<u8>> {
        Queue::new(self.backend.clone(), endpoint, TransferType::Interrupt)
    }

    /// Clear a bulk or interrupt endpoint's halt / stall condition.
    ///
    /// Sends a `CLEAR_FEATURE` `ENDPOINT_HALT` control transfer to tell the
    /// device to reset the endpoint's data toggle and clear the halt / stall
    /// condition, and resets the host-side data toggle.
    ///
    /// Use this after receiving [`TransferError::Stall`] to clear the error and
    /// resume use of the endpoint.
    ///
    /// This should not be called when transfers are pending on the endpoint.
    pub fn clear_halt(&self, endpoint: u8) -> impl MaybeFuture<Output = Result<(), Error>> {
        self.backend.clone().clear_halt(endpoint)
    }

    /// Get the interface number.
    pub fn interface_number(&self) -> u8 {
        self.backend.interface_number
    }

    /// Get the interface descriptors for the alternate settings of this interface.
    ///
    /// This returns cached data and does not perform IO.
    pub fn descriptors(&self) -> impl Iterator<Item = InterfaceDescriptor> {
        let active = self.backend.device.active_configuration_value();

        let configuration = self
            .backend
            .device
            .configuration_descriptors()
            .find(|c| c.configuration_value() == active);

        configuration
            .into_iter()
            .flat_map(|i| i.interface_alt_settings())
            .filter(|g| g.interface_number() == self.backend.interface_number)
    }

    /// Get the interface descriptor for the current alternate setting.
    pub fn descriptor(&self) -> Option<InterfaceDescriptor> {
        self.descriptors()
            .find(|i| i.alternate_setting() == self.get_alt_setting())
    }
}

#[test]
fn assert_send_sync() {
    fn require_send_sync<T: Send + Sync>() {}
    require_send_sync::<Interface>();
    require_send_sync::<Device>();
}
