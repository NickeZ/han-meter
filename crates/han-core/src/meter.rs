use core::fmt;

/// Exact non-negative decimal. Direction is encoded by separate import/export OBIS codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decimal {
    coefficient: u64,
    scale: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidDecimal;

impl Decimal {
    pub fn parse(input: &str) -> Result<Self, InvalidDecimal> {
        let mut coefficient = 0u64;
        let mut scale = 0u8;
        let mut fractional = false;
        let mut digits = 0;
        for byte in input.bytes() {
            match byte {
                b'.' if !fractional && digits > 0 => fractional = true,
                b'0'..=b'9' => {
                    coefficient = coefficient
                        .checked_mul(10)
                        .and_then(|n| n.checked_add(u64::from(byte - b'0')))
                        .ok_or(InvalidDecimal)?;
                    digits += 1;
                    if fractional {
                        scale += 1;
                        if scale > 9 {
                            return Err(InvalidDecimal);
                        }
                    }
                }
                _ => return Err(InvalidDecimal),
            }
        }
        if digits == 0 || (fractional && scale == 0) {
            return Err(InvalidDecimal);
        }
        Ok(Self { coefficient, scale })
    }
}

impl fmt::Display for Decimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.scale == 0 {
            return write!(f, "{}", self.coefficient);
        }
        let divisor = 10u64.pow(u32::from(self.scale));
        write!(
            f,
            "{}.{:0width$}",
            self.coefficient / divisor,
            self.coefficient % divisor,
            width = usize::from(self.scale)
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value {
    Number(Decimal),
    /// Raw meter-local YYMMDDhhmmssW/S. Deliberately not converted to UTC.
    Timestamp([u8; 13]),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(n) => n.fmt(f),
            Self::Timestamp(t) => f.write_str(core::str::from_utf8(t).map_err(|_| fmt::Error)?),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Field {
    pub obis: &'static str,
    pub key: &'static str,
    pub name: &'static str,
    pub wire_unit: Option<&'static str>,
    pub unit: Option<&'static str>,
    pub device_class: Option<&'static str>,
    pub state_class: Option<&'static str>,
}

macro_rules! field {
    ($obis:literal, $key:literal, $name:literal, $wire:literal, $unit:literal, $class:literal, $state:literal) => {
        Field {
            obis: $obis,
            key: $key,
            name: $name,
            wire_unit: Some($wire),
            unit: Some($unit),
            device_class: Some($class),
            state_class: Some($state),
        }
    };
}

pub const FIELD_COUNT: usize = 27;
pub const FIELDS: [Field; FIELD_COUNT] = [
    field!(
        "1-0:1.8.0",
        "energy_import",
        "Energy import",
        "kWh",
        "kWh",
        "energy",
        "total_increasing"
    ),
    field!(
        "1-0:2.8.0",
        "energy_export",
        "Energy export",
        "kWh",
        "kWh",
        "energy",
        "total_increasing"
    ),
    field!(
        "1-0:3.8.0",
        "reactive_energy_import",
        "Reactive energy import",
        "kVArh",
        "kvarh",
        "reactive_energy",
        "total_increasing"
    ),
    field!(
        "1-0:4.8.0",
        "reactive_energy_export",
        "Reactive energy export",
        "kVArh",
        "kvarh",
        "reactive_energy",
        "total_increasing"
    ),
    field!(
        "1-0:1.7.0",
        "power_import",
        "Power import",
        "kW",
        "kW",
        "power",
        "measurement"
    ),
    field!(
        "1-0:2.7.0",
        "power_export",
        "Power export",
        "kW",
        "kW",
        "power",
        "measurement"
    ),
    field!(
        "1-0:3.7.0",
        "reactive_power_import",
        "Reactive power import",
        "kVAr",
        "kvar",
        "reactive_power",
        "measurement"
    ),
    field!(
        "1-0:4.7.0",
        "reactive_power_export",
        "Reactive power export",
        "kVAr",
        "kvar",
        "reactive_power",
        "measurement"
    ),
    field!(
        "1-0:21.7.0",
        "l1_power_import",
        "L1 power import",
        "kW",
        "kW",
        "power",
        "measurement"
    ),
    field!(
        "1-0:22.7.0",
        "l1_power_export",
        "L1 power export",
        "kW",
        "kW",
        "power",
        "measurement"
    ),
    field!(
        "1-0:41.7.0",
        "l2_power_import",
        "L2 power import",
        "kW",
        "kW",
        "power",
        "measurement"
    ),
    field!(
        "1-0:42.7.0",
        "l2_power_export",
        "L2 power export",
        "kW",
        "kW",
        "power",
        "measurement"
    ),
    field!(
        "1-0:61.7.0",
        "l3_power_import",
        "L3 power import",
        "kW",
        "kW",
        "power",
        "measurement"
    ),
    field!(
        "1-0:62.7.0",
        "l3_power_export",
        "L3 power export",
        "kW",
        "kW",
        "power",
        "measurement"
    ),
    field!(
        "1-0:23.7.0",
        "l1_reactive_power_import",
        "L1 reactive power import",
        "kVAr",
        "kvar",
        "reactive_power",
        "measurement"
    ),
    field!(
        "1-0:24.7.0",
        "l1_reactive_power_export",
        "L1 reactive power export",
        "kVAr",
        "kvar",
        "reactive_power",
        "measurement"
    ),
    field!(
        "1-0:43.7.0",
        "l2_reactive_power_import",
        "L2 reactive power import",
        "kVAr",
        "kvar",
        "reactive_power",
        "measurement"
    ),
    field!(
        "1-0:44.7.0",
        "l2_reactive_power_export",
        "L2 reactive power export",
        "kVAr",
        "kvar",
        "reactive_power",
        "measurement"
    ),
    field!(
        "1-0:63.7.0",
        "l3_reactive_power_import",
        "L3 reactive power import",
        "kVAr",
        "kvar",
        "reactive_power",
        "measurement"
    ),
    field!(
        "1-0:64.7.0",
        "l3_reactive_power_export",
        "L3 reactive power export",
        "kVAr",
        "kvar",
        "reactive_power",
        "measurement"
    ),
    field!(
        "1-0:32.7.0",
        "l1_voltage",
        "L1 voltage",
        "V",
        "V",
        "voltage",
        "measurement"
    ),
    field!(
        "1-0:52.7.0",
        "l2_voltage",
        "L2 voltage",
        "V",
        "V",
        "voltage",
        "measurement"
    ),
    field!(
        "1-0:72.7.0",
        "l3_voltage",
        "L3 voltage",
        "V",
        "V",
        "voltage",
        "measurement"
    ),
    field!(
        "1-0:31.7.0",
        "l1_current",
        "L1 current",
        "A",
        "A",
        "current",
        "measurement"
    ),
    field!(
        "1-0:51.7.0",
        "l2_current",
        "L2 current",
        "A",
        "A",
        "current",
        "measurement"
    ),
    field!(
        "1-0:71.7.0",
        "l3_current",
        "L3 current",
        "A",
        "A",
        "current",
        "measurement"
    ),
    Field {
        obis: "0-0:1.0.0",
        key: "meter_timestamp",
        name: "Meter timestamp",
        wire_unit: None,
        unit: None,
        device_class: None,
        state_class: None,
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub readings: [Option<Value>; FIELD_COUNT],
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            readings: [None; FIELD_COUNT],
        }
    }
}
