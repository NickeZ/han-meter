use alloc::string::String;
use core::cell::RefCell;
use embassy_net::Stack;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::{Duration, Timer};
use embedded_storage::nor_flash::ReadNorFlash;
use esp_bootloader_esp_idf::partitions::{PARTITION_TABLE_MAX_LEN, read_partition_table};
use esp_println::println;
use esp_storage::FlashStorage;
use han_config::{
    MqttSettings,
    control::Control,
    storage::{Error as StorageError, SECTOR_SIZE, STORAGE_SIZE, Store},
    web,
};
use static_cell::StaticCell;

pub(crate) type SettingsControl = Control<CriticalSectionRawMutex>;
type SettingsStore = Store<FlashStorage<'static>>;

pub(crate) fn initialize(
    mut flash: FlashStorage<'static>,
) -> (&'static SettingsControl, Option<SettingsStore>) {
    let defaults = MqttSettings::new(
        crate::config::MQTT_HOST,
        crate::config::MQTT_PORT,
        crate::config::MQTT_USERNAME,
        crate::config::MQTT_PASSWORD,
    )
    .expect("Validated MQTT defaults");
    let store = match storage_offset(&mut flash).and_then(|offset| {
        Store::open(flash, offset).map_err(|error| {
            println!(
                "MQTT settings journal at {:#x} could not be opened: {:?}",
                offset, error
            );
            match error {
                StorageError::ForeignData => {
                    println!(
                        "To discard the old settings region, exit the USB monitor and run: espflash erase-region --chip esp32c3 --port /dev/ttyACM0 {:#x} {:#x}",
                        offset, STORAGE_SIZE
                    );
                    "nvs contains unrecognized data or an unsupported settings version; ordinary reflashing preserves it"
                }
                StorageError::Region => "nvs journal geometry is incompatible with flash storage",
                _ => "could not read the nvs settings journal",
            }
        })
    }) {
        Ok(store) => Some(store),
        Err(reason) => {
            println!(
                "MQTT settings storage unavailable: {}. Using firmware defaults; web saving disabled.",
                reason
            );
            None
        }
    };
    let settings = store
        .as_ref()
        .and_then(Store::load)
        .cloned()
        .unwrap_or(defaults);
    println!(
        "MQTT settings source: {}",
        if store.as_ref().and_then(Store::load).is_some() {
            "saved"
        } else {
            "firmware defaults"
        }
    );
    static CONTROL: StaticCell<SettingsControl> = StaticCell::new();
    (CONTROL.init(Control::new(settings)), store)
}

/// Use the partition already reserved by espflash. Never guess a flash-tail address
/// or write across an app, bootloader, partition table, or another data partition.
fn storage_offset(flash: &mut FlashStorage<'_>) -> Result<u32, &'static str> {
    let capacity = ReadNorFlash::capacity(flash) as u32;
    let mut bytes = [0; PARTITION_TABLE_MAX_LEN];
    let table = read_partition_table(flash, &mut bytes).map_err(|error| {
        println!("MQTT settings partition table error: {:?}", error);
        "could not read or validate the partition table"
    })?;
    let mut region = None;
    for entry in table.iter() {
        if entry.label().split(|&b| b == 0 || b == 0xff).next() != Some(b"nvs".as_slice()) {
            continue;
        }
        if entry.raw_type() != 1
            || entry.raw_subtype() != 2
            || entry.flags() != 0
            || entry.offset() < 0x9000
            || !entry.offset().is_multiple_of(SECTOR_SIZE)
            || entry.len() < STORAGE_SIZE
            || entry
                .offset()
                .checked_add(entry.len())
                .is_none_or(|end| end > capacity)
        {
            println!(
                "MQTT settings nvs partition: type={:#x} subtype={:#x} flags={:#x} offset={:#x} length={:#x} flash_capacity={:#x}",
                entry.raw_type(),
                entry.raw_subtype(),
                entry.flags(),
                entry.offset(),
                entry.len(),
                capacity
            );
            return Err("nvs partition has an unsupported type, flags, alignment, or size");
        }
        if region.replace(entry.offset()).is_some() {
            return Err("multiple nvs partitions found");
        }
    }
    let offset = region.ok_or("nvs partition is missing")?;
    for entry in table.iter() {
        if entry.offset() == offset {
            // Only the selected nvs entry may cover the journal.
            if entry.label().split(|&b| b == 0 || b == 0xff).next() != Some(b"nvs".as_slice()) {
                return Err("another partition overlaps the nvs settings journal");
            }
            continue;
        }
        let end = entry
            .offset()
            .checked_add(entry.len())
            .ok_or("partition address overflows flash address space")?;
        if entry.offset() < offset + STORAGE_SIZE && end > offset {
            return Err("another partition overlaps the nvs settings journal");
        }
    }
    Ok(offset)
}

struct HttpApplication {
    control: &'static SettingsControl,
    store: RefCell<Option<SettingsStore>>,
    stack: Stack<'static>,
    hostname: String,
}

impl web::Application for HttpApplication {
    fn snapshot(&self) -> han_config::control::Snapshot {
        self.control.snapshot()
    }

    fn save(&self, settings: MqttSettings) -> Result<bool, web::SaveError> {
        if self.control.snapshot().settings == settings {
            return Ok(false);
        }
        let mut store = self.store.borrow_mut();
        let store = store.as_mut().ok_or(web::SaveError)?;
        store.save(&settings).map_err(|error| {
            println!(
                "MQTT settings save failed: {:?}; active settings unchanged",
                error
            );
            web::SaveError
        })?;
        let changed = self.control.apply(settings);
        println!("MQTT settings saved");
        Ok(changed)
    }

    fn storage_available(&self) -> bool {
        self.store
            .borrow()
            .as_ref()
            .is_some_and(Store::is_available)
    }

    fn host_allowed(&self, host: &str) -> bool {
        let host = host.strip_suffix(":80").unwrap_or(host);
        if host.eq_ignore_ascii_case(&self.hostname) {
            return true;
        }
        self.stack.config_v4().is_some_and(|config| {
            host.parse::<embassy_net::Ipv4Address>().ok() == Some(config.address.address())
        })
    }
}

#[embassy_executor::task]
pub(crate) async fn http(
    stack: Stack<'static>,
    control: &'static SettingsControl,
    store: Option<SettingsStore>,
    hostname: String,
) {
    let app = HttpApplication {
        control,
        store: RefCell::new(store),
        stack,
        hostname,
    };
    let router = web::router(&app);
    let config = picoserve::Config::new(picoserve::Timeouts {
        start_read_request: Duration::from_secs(5),
        persistent_start_read_request: Duration::from_secs(1),
        read_request: Duration::from_secs(5),
        write: Duration::from_secs(5),
    })
    .close_connection_after_response();
    let mut http_buffer = [0; web::HTTP_BUFFER_SIZE];
    let mut rx = [0; 2048];
    let mut tx = [0; 2048];
    picoserve::Server::new(&router, &config, &mut http_buffer)
        .listen_and_serve(0, stack, 80, &mut rx, &mut tx)
        .await;
}

#[embassy_executor::task]
pub(crate) async fn address_log(stack: Stack<'static>) {
    let mut previous = None;
    loop {
        let address = stack.config_v4().map(|config| config.address.address());
        if address != previous {
            if let Some(address) = address {
                println!("MQTT settings page: http://{}/", address);
            }
            previous = address;
        }
        Timer::after(Duration::from_secs(1)).await;
    }
}
