use core::borrow::BorrowMut;
use core::mem;
use core::slice;
use usb_device::class_prelude::*;
use usb_device::Result;
use crate::cdc_acm::*;
use crate::buffer::{Buffer, DefaultBufferStore};

/// USB (CDC-ACM) serial port with built-in buffering to implement stream-like behavior.
///
/// The RS and WS type arguments specify the storage for the read/write buffers, respectively. By
/// default an interna 128 byte buffer is used for both directions.
pub struct SerialPort<'a, B, RS=DefaultBufferStore, WS=DefaultBufferStore>
where
    B: UsbBus,
    RS: BorrowMut<[u8]>,
    WS: BorrowMut<[u8]>,
{
    inner: CdcAcmClass<'a, B>,
    read_buf: Buffer<RS>,
    write_buf: Buffer<WS>,
    need_zlp: bool,
}

impl<B> SerialPort<'_, B>
where
    B: UsbBus
{
    /// Creates a new USB serial port with the provided UsbBus and 128 byte read/write buffers.
    pub fn new(alloc: &UsbBusAllocator<B>)
        -> SerialPort<'_, B, DefaultBufferStore, DefaultBufferStore>
    {
        SerialPort {
            inner: CdcAcmClass::new(alloc, 64),
            read_buf: Buffer::new(unsafe { mem::uninitialized() }),
            write_buf: Buffer::new(unsafe { mem::uninitialized() }),
            need_zlp: false,
        }
    }
}

impl<B, RS, WS> SerialPort<'_, B, RS, WS>
where
    B: UsbBus,
    RS: BorrowMut<[u8]>,
    WS: BorrowMut<[u8]>,
{
    /// Creates a new USB serial port with the provided UsbBus and buffer backing stores.
    pub fn new_with_store(alloc: &UsbBusAllocator<B>, read_store: RS, write_store: WS)
        -> SerialPort<'_, B, RS, WS>
    {
        SerialPort {
            inner: CdcAcmClass::new(alloc, 64),
            read_buf: Buffer::new(read_store),
            write_buf: Buffer::new(write_store),
            need_zlp: false,
        }
    }

    /// Gets the current line coding.
    pub fn line_coding(&self) -> &LineCoding { self.inner.line_coding() }

    /// Gets the DTR (data terminal ready) state
    pub fn dtr(&self) -> bool { self.inner.dtr() }

    /// Gets the RTS (ready to send) state
    pub fn rts(&self) -> bool { self.inner.rts() }

    /// Writes bytes from `data` into the port and returns the number of bytes written.
    pub fn write(&mut self, data: &[u8]) -> Result<usize> {
        if self.write_buf.available_write() == 0 {
            // Buffer is full, try to flush

            match self.flush() {
                Ok(_) | Err(UsbError::WouldBlock) => { },
                Err(err) => { return Err(err); },
            };

            if self.write_buf.available_write() == 0 {
                // Still full, can't write anything.
                return Ok(0);
            }
        }

        Ok(self.write_buf.write(data))
    }

    /// Reads bytes from the port into `data` and returns the number of bytes read.
    pub fn read(&mut self, data: &mut [u8]) -> Result<usize> {
        let buf = &mut self.read_buf;
        let inner = &mut self.inner;

        // Try to read a packet from the endpoint and write it into the buffer if it fits. Propagate
        // errors except `WouldBlock`.

        buf.write_all(inner.max_packet_size() as usize, |buf_data| {
            match inner.read_packet(buf_data) {
                Ok(c) => Ok(c),
                Err(UsbError::WouldBlock) => Ok(0),
                Err(err) => Err(err),
            }
        })?;

        if buf.available_read() == 0 {
            // No data available for reading.
            return Ok(0);
        }

        let r = buf.read(data.len(), |buf_data| {
            &data[..buf_data.len()].copy_from_slice(buf_data);

            Ok(buf_data.len())
        });

        r
    }

    /// Sends as much as possible of the current write buffer. Returns `Ok` if the write buffer has
    /// been completely transferred to and acknowledged by the host, `Err(WouldBlock)` if there is
    /// still unacknowledged data, and other errors if there's an error sending data to the host.
    pub fn flush(&mut self) -> Result<()> {
        let buf = &mut self.write_buf;
        let inner = &mut self.inner;
        let need_zlp = &mut self.need_zlp;

        if buf.available_read() > 0 {
            buf.read(inner.max_packet_size() as usize, |buf_data| {
                inner.write_packet(buf_data)?;

                *need_zlp = (buf_data.len() == inner.max_packet_size() as usize);

                Ok(buf_data.len())
            })?;

            Err(UsbError::WouldBlock)
        } else if *need_zlp {
            // Write a ZLP to complete the transaction if there's nothing else to write and the last
            // packet was a full one.
            inner.write_packet(&[])?;

            *need_zlp = false;

            Err(UsbError::WouldBlock)
        } else {
            Ok(())
        }
    }
}

impl<B, RS, WS> UsbClass<B> for SerialPort<'_, B, RS, WS>
where
    B: UsbBus,
    RS: BorrowMut<[u8]>,
    WS: BorrowMut<[u8]>,
{
    fn get_configuration_descriptors(&self, writer: &mut DescriptorWriter) -> Result<()> {
        self.inner.get_configuration_descriptors(writer)
    }

    fn reset(&mut self) {
        self.inner.reset();
        self.read_buf.clear();
        self.write_buf.clear();
        self.need_zlp = false;
    }

    fn endpoint_in_complete(&mut self, addr: EndpointAddress) {
        if addr == self.inner.write_ep_address() {
            self.flush().ok();
        }
    }

    fn control_in(&mut self, xfer: ControlIn<B>) { self.inner.control_in(xfer); }

    fn control_out(&mut self, xfer: ControlOut<B>) { self.inner.control_out(xfer); }
}

impl<B, RS, WS> embedded_hal::serial::Write<u8> for SerialPort<'_, B, RS, WS>
where
    B: UsbBus,
    RS: BorrowMut<[u8]>,
    WS: BorrowMut<[u8]>,
{
    type Error = UsbError;

    fn write(&mut self, word: u8) -> nb::Result<(), Self::Error> {
        match <SerialPort<'_, B, RS, WS>>::write(self, slice::from_ref(&word)) {
            Ok(0) | Err(UsbError::WouldBlock) => Err(nb::Error::WouldBlock),
            Ok(_) => Ok(()),
            Err(err) => Err(nb::Error::Other(err)),
        }
    }

    fn flush(&mut self) -> nb::Result<(), Self::Error> {
        match <SerialPort<'_, B, RS, WS>>::flush(self) {
            Err(UsbError::WouldBlock) => Err(nb::Error::WouldBlock),
            Ok(_) => Ok(()),
            Err(err) => Err(nb::Error::Other(err)),
        }
    }
}

impl<B, RS, WS> embedded_hal::serial::Read<u8> for SerialPort<'_, B, RS, WS>
where
    B: UsbBus,
    RS: BorrowMut<[u8]>,
    WS: BorrowMut<[u8]>,
{
    type Error = UsbError;

    fn read(&mut self) -> nb::Result<u8, Self::Error> {
        let mut buf: u8 = 0;

        match <SerialPort<'_, B, RS, WS>>::read(self, slice::from_mut(&mut buf)) {
            Ok(0) | Err(UsbError::WouldBlock) => Err(nb::Error::WouldBlock),
            Ok(_) => Ok(buf),
            Err(err) => Err(nb::Error::Other(err)),
        }
    }
}
