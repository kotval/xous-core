#![no_std]

#[cfg(feature = "swap")]
pub mod swap;

pub const PAGE_SIZE: usize = 4096;
pub const FLG_SWAP_USED: u32 = 0x8000_0000;

// Locate the hard-wired IFRAM allocations for UDMA
#[allow(dead_code)]
#[cfg(feature = "cramium-soc")]
pub const UART_IFRAM_ADDR: usize = utralib::HW_IFRAM0_MEM + utralib::HW_IFRAM0_MEM_LEN - 4096;
#[allow(dead_code)]
#[cfg(feature = "cramium-soc")]
pub const APP_UART_IFRAM_ADDR: usize = utralib::HW_IFRAM0_MEM + utralib::HW_IFRAM0_MEM_LEN - 3 * 4096;
