//! ESP32-C3 ROM download mode; no flash or eFuse writes are needed.
use esp_hal::{efuse, peripherals::LPWR};

pub(crate) fn available() -> bool {
    !efuse::read_bit(efuse::DIS_FORCE_DOWNLOAD)
        && !efuse::read_bit(efuse::DIS_DOWNLOAD_MODE)
        && !efuse::read_bit(efuse::DIS_USB_SERIAL_JTAG)
        && !efuse::read_bit(efuse::DIS_USB_SERIAL_JTAG_DOWNLOAD_MODE)
}

pub(crate) fn enter() -> ! {
    // ESP32-C3 TRM, Chip Boot Control: FORCE_DOWNLOAD_BOOT overrides the boot
    // straps on reset. This dedicated register has no other writable fields
    // and no other application writer. A full SoC reset also stops UART DMA
    // and Wi-Fi before entering the ROM loader.
    // https://documentation.espressif.com/esp32-c3_technical_reference_manual_en.pdf
    LPWR::regs()
        .option1()
        .modify(|_, w| w.force_download_boot().set_bit());
    esp_hal::system::software_reset()
}
