//! Physical device drivers, separate from guest-visible virtual devices.
pub mod interrupt;
pub mod usb;

pub trait Driver {
    type Error;
    fn initialize(&mut self) -> Result<(), Self::Error>;
    /// Service available work with bounded effort; never wait for a host.
    fn poll(&mut self) -> Result<(), Self::Error>;
}

/// Physical interrupt controller used by an architecture exception vector.
/// Taking an interrupt and maintaining its virtual state are both bounded and
/// must never wait for another vCPU.
pub trait InterruptController: Driver {
    type Event;
    fn take_interrupt(&mut self) -> Self::Event;
}

pub trait TxTransport: Driver {
    fn connected(&self) -> bool;
    /// Available staging space, determined without MMIO or copying payload bytes.
    /// Zero means callers should leave their queued bytes untouched. `send` may
    /// still accept fewer bytes because of hardware state or packet pacing.
    fn send_capacity(&self) -> usize;
    /// Nonblocking. Accepted bytes are copied into driver-owned storage.
    fn send(&mut self, bytes: &[u8]) -> Result<usize, Self::Error>;
}

pub trait RxTransport: Driver {
    /// Nonblocking. Copies bytes already owned by the driver into `output`.
    /// Returning zero means that no complete receive data is available.
    fn receive(&mut self, output: &mut [u8]) -> Result<usize, Self::Error>;
}

pub trait Mmio {
    fn read32(&mut self, address: u64) -> u32;
    fn write32(&mut self, address: u64, value: u32);
    fn barrier(&mut self);
}

pub trait Clock {
    fn now_us(&mut self) -> u64;
    fn delay_us(&mut self, duration: u32);
}

/// Identity-addressed non-cacheable DMA storage, exclusively owned by the driver.
pub trait DmaBuffer {
    fn physical_base(&self) -> u64;
    fn size(&self) -> usize;
    fn read32(&mut self, offset: usize) -> u32;
    fn write32(&mut self, offset: usize, value: u32);
    /// Borrow a CPU-owned payload range for the duration of `f`. The driver
    /// calls this only after OUT completion or before publishing an IN TRB,
    /// and cannot rearm/submit the range until `f` returns. Other DMA ranges
    /// (event rings, contexts, other endpoints) may still be hardware-owned.
    /// The adapter must bounds-check and provide a non-cacheable view without
    /// copying. References cannot escape the callback.
    fn with_cpu_buffer<R>(
        &mut self,
        offset: usize,
        length: usize,
        f: impl FnOnce(&mut [u8]) -> R,
    ) -> R;
}
