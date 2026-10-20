use embedded_storage::nor_flash::{
    ErrorType, NorFlash, NorFlashError, NorFlashErrorKind, ReadNorFlash,
};
use han_config::{
    MqttSettings,
    storage::{Error, Store},
};
use std::{cell::Cell, rc::Rc};

struct Flash {
    bytes: Vec<u8>,
    budget: Rc<Cell<Option<usize>>>,
    steps: Rc<Cell<usize>>,
}
impl Clone for Flash {
    fn clone(&self) -> Self {
        Self {
            bytes: self.bytes.clone(),
            budget: Rc::new(Cell::new(None)),
            steps: Rc::new(Cell::new(0)),
        }
    }
}
#[derive(Debug)]
struct Io;
impl NorFlashError for Io {
    fn kind(&self) -> NorFlashErrorKind {
        NorFlashErrorKind::Other
    }
}
impl ErrorType for Flash {
    type Error = Io;
}
impl Flash {
    fn new() -> Self {
        Self {
            bytes: vec![0xff; 4 * 4096],
            budget: Rc::new(Cell::new(None)),
            steps: Rc::new(Cell::new(0)),
        }
    }
    fn step(&mut self) -> Result<(), Io> {
        if self.budget.get() == Some(0) {
            return Err(Io);
        }
        if let Some(budget) = self.budget.get() {
            self.budget.set(Some(budget - 1));
        }
        self.steps.set(self.steps.get() + 1);
        Ok(())
    }
}
impl ReadNorFlash for Flash {
    const READ_SIZE: usize = 4;
    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Io> {
        self.step()?;
        assert!((offset as usize).is_multiple_of(4) && bytes.len().is_multiple_of(4));
        bytes.copy_from_slice(&self.bytes[offset as usize..offset as usize + bytes.len()]);
        Ok(())
    }
    fn capacity(&self) -> usize {
        self.bytes.len()
    }
}
impl NorFlash for Flash {
    const WRITE_SIZE: usize = 4;
    const ERASE_SIZE: usize = 4096;
    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Io> {
        assert!((offset as usize).is_multiple_of(4) && bytes.len().is_multiple_of(4));
        for (i, word) in bytes.chunks_exact(4).enumerate() {
            self.step()?;
            for (j, &value) in word.iter().enumerate() {
                let byte = &mut self.bytes[offset as usize + i * 4 + j];
                assert_eq!(*byte & value, value, "must erase before writing");
                *byte = value;
            }
        }
        Ok(())
    }
    fn erase(&mut self, from: u32, to: u32) -> Result<(), Io> {
        assert!(from.is_multiple_of(4096) && to.is_multiple_of(4096));
        for offset in (from as usize..to as usize).step_by(4) {
            self.step()?;
            self.bytes[offset..offset + 4].fill(0xff);
        }
        Ok(())
    }
}
fn settings(port: u16) -> MqttSettings {
    MqttSettings::new("mqtt.example.net", port, "meter", "secret").unwrap()
}

#[test]
fn ipv6_broker_settings_survive_reopening_the_existing_journal_format() {
    let mut store = Store::open(Flash::new(), 4096).unwrap();
    for host in ["fd12:3456:789a::10", "[2001:db8::80]", "fe80::1234"] {
        let settings = MqttSettings::new(host, 1883, "meter", "secret").unwrap();
        assert!(store.save(&settings).unwrap());
        store = Store::open(store.into_inner(), 4096).unwrap();
        assert!(store.load() == Some(&settings));
    }
}

#[test]
fn persists_and_skips_unchanged_writes_without_touching_other_sectors() {
    let mut store = Store::open(Flash::new(), 4096).unwrap();
    assert!(store.load().is_none());
    assert!(store.save(&settings(1883)).unwrap());
    let flash = store.into_inner();
    let steps = flash.steps.clone();
    let budget = flash.budget.clone();
    let mut store = Store::open(flash, 4096).unwrap();
    assert!(store.load() == Some(&settings(1883)));
    let before = steps.get();
    budget.set(Some(0));
    assert!(!store.save(&settings(1883)).unwrap());
    assert_eq!(steps.get(), before);
    budget.set(None);
    store.save(&settings(1884)).unwrap();
    let flash = store.into_inner();
    assert!(flash.bytes[..4096].iter().all(|&v| v == 0xff));
    assert!(flash.bytes[3 * 4096..].iter().all(|&v| v == 0xff));
    let store = Store::open(flash, 4096).unwrap();
    assert!(store.load() == Some(&settings(1884)));
}

#[test]
fn power_loss_at_every_write_erase_and_verification_boundary_preserves_a_complete_record() {
    let mut store = Store::open(Flash::new(), 4096).unwrap();
    store.save(&settings(1883)).unwrap();
    store.save(&settings(1884)).unwrap();
    let baseline = store.into_inner();
    // Erase 1024 words, write 168 words, verify, commit, verify.
    for budget in 0..=1195 {
        let flash = baseline.clone();
        let fault = flash.budget.clone();
        let mut store = Store::open(flash, 4096).unwrap();
        fault.set(Some(budget));
        let _ = store.save(&settings(1885));
        let flash = store.into_inner();
        fault.set(None);
        let mut recovered = Store::open(flash, 4096).unwrap();
        assert!(
            recovered.load() == Some(&settings(1884)) || recovered.load() == Some(&settings(1885)),
            "power cut at step {budget}"
        );
        recovered.save(&settings(1886)).unwrap();
        assert!(Store::open(recovered.into_inner(), 4096).unwrap().load() == Some(&settings(1886)));
    }
}

#[test]
fn flash_errors_keep_current_settings_and_disable_further_writes_until_reopen() {
    let flash = Flash::new();
    let fault = flash.budget.clone();
    let mut store = Store::open(flash, 4096).unwrap();
    store.save(&settings(1883)).unwrap();
    fault.set(Some(1025));
    assert_eq!(store.save(&settings(1884)), Err(Error::Io));
    assert!(!store.is_available());
    assert!(store.load() == Some(&settings(1883)));
    fault.set(None);
    assert_eq!(store.save(&settings(1884)), Err(Error::Io));
    let mut store = Store::open(store.into_inner(), 4096).unwrap();
    store.save(&settings(1884)).unwrap();
}

#[test]
fn corruption_falls_back_and_unknown_regions_are_not_written() {
    let mut store = Store::open(Flash::new(), 4096).unwrap();
    store.save(&settings(1883)).unwrap();
    store.save(&settings(1884)).unwrap();
    let mut flash = store.into_inner();
    flash.bytes[8192 + 40] ^= 1;
    let store = Store::open(flash, 4096).unwrap();
    assert!(store.load() == Some(&settings(1883)));
    let mut flash = store.into_inner();
    flash.bytes[4096 + 40] ^= 1;
    assert!(Store::open(flash, 4096).unwrap().load().is_none());
    let mut foreign = Flash::new();
    foreign.bytes[4096 + 2048] = 0;
    assert!(matches!(
        Store::open(foreign, 4096),
        Err(Error::ForeignData)
    ));
    assert!(matches!(Store::open(Flash::new(), 1), Err(Error::Region)));
    assert!(matches!(
        Store::open(Flash::new(), 3 * 4096),
        Err(Error::Region)
    ));
}
