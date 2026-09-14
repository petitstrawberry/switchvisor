//! Volatile physical MMIO, architectural time and protected non-cacheable DMA.
use core::{arch::asm, cell::UnsafeCell};
use switchvisor::drivers::{Clock, DmaBuffer, Mmio, usb::tegra210::DMA_SIZE};

pub struct Hardware;
impl Mmio for Hardware {
    fn read32(&mut self, address: u64) -> u32 {
        unsafe { core::ptr::read_volatile(address as *const u32) }
    }
    fn write32(&mut self, address: u64, value: u32) {
        unsafe {
            core::ptr::write_volatile(address as *mut u32, value);
        }
    }
    fn barrier(&mut self) {
        unsafe {
            asm!("dsb sy", options(nostack));
        }
    }
}
impl Clock for Hardware {
    fn now_us(&mut self) -> u64 {
        let count: u64;
        let frequency: u64;
        unsafe {
            asm!("mrs {count}, cntpct_el0", "mrs {frequency}, cntfrq_el0", count = out(reg) count, frequency = out(reg) frequency, options(nomem, nostack));
        }
        assert!(frequency != 0);
        (count / frequency) * 1_000_000 + (count % frequency) * 1_000_000 / frequency
    }
    fn delay_us(&mut self, duration: u32) {
        let start = self.now_us();
        while self.now_us().wrapping_sub(start) < u64::from(duration) {
            core::hint::spin_loop();
        }
    }
}

#[repr(C, align(4096))]
struct Storage(UnsafeCell<[u32; DMA_SIZE / 4]>);
// Only the VM console transport mutex owner accesses the buffer; hardware DMA uses volatile words.
unsafe impl Sync for Storage {}
static STORAGE: Storage = Storage(UnsafeCell::new([0; DMA_SIZE / 4]));
pub struct UsbDma;
impl DmaBuffer for UsbDma {
    fn physical_base(&self) -> u64 {
        STORAGE.0.get() as u64
    }
    fn size(&self) -> usize {
        DMA_SIZE
    }
    fn read32(&mut self, offset: usize) -> u32 {
        assert!(offset % 4 == 0 && offset <= DMA_SIZE - 4);
        unsafe { core::ptr::read_volatile(STORAGE.0.get().cast::<u32>().add(offset / 4)) }
    }
    fn write32(&mut self, offset: usize, value: u32) {
        assert!(offset % 4 == 0 && offset <= DMA_SIZE - 4);
        unsafe {
            core::ptr::write_volatile(STORAGE.0.get().cast::<u32>().add(offset / 4), value);
        }
    }
}
