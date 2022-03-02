//! IEEE 802.15.4 radio driver for nRF52

use core::cell::Cell;
use core::convert::TryFrom;
use kernel;
use kernel::hil::radio::{self, PowerClient, RadioConfig, RadioData};
use kernel::hil::time::{Alarm, AlarmClient};
use kernel::utilities::cells::{OptionalCell, TakeCell};
use kernel::utilities::registers::interfaces::{Readable, Writeable};
use kernel::utilities::registers::{register_bitfields, ReadOnly, ReadWrite, WriteOnly};
use kernel::utilities::StaticRef;
use kernel::ErrorCode;

use nrf5x;
use nrf5x::constants::TxPower;

// This driver has some significant flaws -- no ACK support, power cycles
// the radio after every transmission or reception,
// doesn't always check hardware for errors and instead defaults to
// returning Ok(()). However as of 05/26/20 it does interoperate
// with other 15.4 implementations for Tock's basic 15.4 apps.

const RADIO_BASE: StaticRef<RadioRegisters> =
    unsafe { StaticRef::new(0x40001000 as *const RadioRegisters) };

pub const IEEE802154_PAYLOAD_LENGTH: usize = 255;
pub const IEEE802154_BACKOFF_PERIOD: usize = 320; //microseconds = 20 symbols
pub const IEEE802154_ACK_TIME: usize = 512; //microseconds = 32 symbols
pub const IEEE802154_MAX_POLLING_ATTEMPTS: u8 = 4;
pub const IEEE802154_MIN_BE: u8 = 3;
pub const IEEE802154_MAX_BE: u8 = 5;
pub const RAM_LEN_BITS: usize = 8;
pub const RAM_S1_BITS: usize = 0;
pub const PREBUF_LEN_BYTES: usize = 2;

// artifact of entanglement with rf233 implementation, mac layer
// places packet data starting PSDU_OFFSET=2 bytes after start of
// buffer to make room for 1 byte spi header required when communicating
// with rf233 over SPI. nrf radio does not need this header, so we
// have to pretend the frame buffer starts 1 byte later than the
// frame passed by the mac layer. We can't just drop the byte from
// the buffer because then it would be lost forever when we tried
// to return the frame buffer.
const MIMIC_PSDU_OFFSET: u32 = 1;

// IEEEStd 802.15.4-2011 Section 8.1.2.2
// Frequency is 2405 + 5 * (k - 11) MHz, where k = 11, 12, ... , 26.
#[derive(PartialEq, Debug, Copy, Clone)]
pub enum RadioChannel {
    DataChannel11 = 5,
    DataChannel12 = 10,
    DataChannel13 = 15,
    DataChannel14 = 20,
    DataChannel15 = 25,
    DataChannel16 = 30,
    DataChannel17 = 35,
    DataChannel18 = 40,
    DataChannel19 = 45,
    DataChannel20 = 50,
    DataChannel21 = 55,
    DataChannel22 = 60,
    DataChannel23 = 65,
    DataChannel24 = 70,
    DataChannel25 = 75,
    DataChannel26 = 80,
}

impl RadioChannel {
    pub fn get_channel_index(&self) -> u8 {
        match *self {
            RadioChannel::DataChannel11 => 11,
            RadioChannel::DataChannel12 => 12,
            RadioChannel::DataChannel13 => 13,
            RadioChannel::DataChannel14 => 14,
            RadioChannel::DataChannel15 => 15,
            RadioChannel::DataChannel16 => 16,
            RadioChannel::DataChannel17 => 17,
            RadioChannel::DataChannel18 => 18,
            RadioChannel::DataChannel19 => 19,
            RadioChannel::DataChannel20 => 20,
            RadioChannel::DataChannel21 => 21,
            RadioChannel::DataChannel22 => 22,
            RadioChannel::DataChannel23 => 23,
            RadioChannel::DataChannel24 => 24,
            RadioChannel::DataChannel25 => 25,
            RadioChannel::DataChannel26 => 26,
        }
    }
}

impl TryFrom<u8> for RadioChannel {
    type Error = ();

    fn try_from(val: u8) -> Result<RadioChannel, ()> {
        match val {
            11 => Ok(RadioChannel::DataChannel11),
            12 => Ok(RadioChannel::DataChannel12),
            13 => Ok(RadioChannel::DataChannel13),
            14 => Ok(RadioChannel::DataChannel14),
            15 => Ok(RadioChannel::DataChannel15),
            16 => Ok(RadioChannel::DataChannel16),
            17 => Ok(RadioChannel::DataChannel17),
            18 => Ok(RadioChannel::DataChannel18),
            19 => Ok(RadioChannel::DataChannel19),
            20 => Ok(RadioChannel::DataChannel20),
            21 => Ok(RadioChannel::DataChannel21),
            22 => Ok(RadioChannel::DataChannel22),
            23 => Ok(RadioChannel::DataChannel23),
            24 => Ok(RadioChannel::DataChannel24),
            25 => Ok(RadioChannel::DataChannel25),
            26 => Ok(RadioChannel::DataChannel26),
            _ => Err(()),
        }
    }
}

#[repr(C)]
struct RadioRegisters {
    /// Enable Radio in TX mode
    /// - Address: 0x000 - 0x004
    task_txen: WriteOnly<u32, Task::Register>,
    /// Enable Radio in RX mode
    /// - Address: 0x004 - 0x008
    task_rxen: WriteOnly<u32, Task::Register>,
    /// Start Radio
    /// - Address: 0x008 - 0x00c
    task_start: WriteOnly<u32, Task::Register>,
    /// Stop Radio
    /// - Address: 0x00c - 0x010
    task_stop: WriteOnly<u32, Task::Register>,
    /// Disable Radio
    /// - Address: 0x010 - 0x014
    task_disable: WriteOnly<u32, Task::Register>,
    /// Start the RSSI and take one single sample of the receive signal strength
    /// - Address: 0x014- 0x018
    task_rssistart: WriteOnly<u32, Task::Register>,
    /// Stop the RSSI measurement
    /// - Address: 0x018 - 0x01c
    task_rssistop: WriteOnly<u32, Task::Register>,
    /// Start the bit counter
    /// - Address: 0x01c - 0x020
    task_bcstart: WriteOnly<u32, Task::Register>,
    /// Stop the bit counter
    /// - Address: 0x020 - 0x024
    task_bcstop: WriteOnly<u32, Task::Register>,
    /// Reserved
    _reserved1: [u32; 2],
    /// Stop the bit counter
    /// - Address: 0x02c - 0x030
    task_ccastart: WriteOnly<u32, Task::Register>,
    /// Stop the bit counter
    /// - Address: 0x030 - 0x034
    task_ccastop: WriteOnly<u32, Task::Register>,
    /// Reserved
    _reserved2: [u32; 51],
    /// Radio has ramped up and is ready to be started
    /// - Address: 0x100 - 0x104
    event_ready: ReadWrite<u32, Event::Register>,
    /// Address sent or received
    /// - Address: 0x104 - 0x108
    event_address: ReadWrite<u32, Event::Register>,
    /// Packet payload sent or received
    /// - Address: 0x108 - 0x10c
    event_payload: ReadWrite<u32, Event::Register>,
    /// Packet sent or received
    /// - Address: 0x10c - 0x110
    event_end: ReadWrite<u32, Event::Register>,
    /// Radio has been disabled
    /// - Address: 0x110 - 0x114
    event_disabled: ReadWrite<u32, Event::Register>,
    /// A device address match occurred on the last received packet
    /// - Address: 0x114 - 0x118
    event_devmatch: ReadWrite<u32>,
    /// No device address match occurred on the last received packet
    /// - Address: 0x118 - 0x11c
    event_devmiss: ReadWrite<u32, Event::Register>,
    /// Sampling of receive signal strength complete
    /// - Address: 0x11c - 0x120
    event_rssiend: ReadWrite<u32, Event::Register>,
    /// Reserved
    _reserved3: [u32; 2],
    /// Bit counter reached bit count value
    /// - Address: 0x128 - 0x12c
    event_bcmatch: ReadWrite<u32, Event::Register>,
    /// Reserved
    _reserved4: [u32; 1],
    /// Packet received with CRC ok
    /// - Address: 0x130 - 0x134
    event_crcok: ReadWrite<u32, Event::Register>,
    /// Packet received with CRC error
    /// - Address: 0x134 - 0x138
    crcerror: ReadWrite<u32, Event::Register>,
    /// IEEE 802.15.4 length field received
    /// - Address: 0x138 - 0x13c
    event_framestart: ReadWrite<u32, Event::Register>,
    /// Reserved
    _reserved5: [u32; 2],
    /// Wireless medium in idle - clear to send
    /// - Address: 0x144-0x148
    event_ccaidle: ReadWrite<u32, Event::Register>,
    /// Wireless medium busy - do not send
    /// - Address: 0x148-0x14c
    event_ccabusy: ReadWrite<u32, Event::Register>,
    /// Reserved
    _reserved6: [u32; 45],
    /// Shortcut register
    /// - Address: 0x200 - 0x204
    shorts: ReadWrite<u32, Shortcut::Register>,
    /// Reserved
    _reserved7: [u32; 64],
    /// Enable interrupt
    /// - Address: 0x304 - 0x308
    intenset: ReadWrite<u32, Interrupt::Register>,
    /// Disable interrupt
    /// - Address: 0x308 - 0x30c
    intenclr: ReadWrite<u32, Interrupt::Register>,
    /// Reserved
    _reserved8: [u32; 61],
    /// CRC status
    /// - Address: 0x400 - 0x404
    crcstatus: ReadOnly<u32, Event::Register>,
    /// Reserved
    _reserved9: [u32; 1],
    /// Received address
    /// - Address: 0x408 - 0x40c
    rxmatch: ReadOnly<u32, ReceiveMatch::Register>,
    /// CRC field of previously received packet
    /// - Address: 0x40c - 0x410
    rxcrc: ReadOnly<u32, ReceiveCrc::Register>,
    /// Device address match index
    /// - Address: 0x410 - 0x414
    dai: ReadOnly<u32, DeviceAddressIndex::Register>,
    /// Reserved
    _reserved10: [u32; 60],
    /// Packet pointer
    /// - Address: 0x504 - 0x508
    packetptr: ReadWrite<u32, PacketPointer::Register>,
    /// Frequency
    /// - Address: 0x508 - 0x50c
    frequency: ReadWrite<u32, Frequency::Register>,
    /// Output power
    /// - Address: 0x50c - 0x510
    txpower: ReadWrite<u32, TransmitPower::Register>,
    /// Data rate and modulation
    /// - Address: 0x510 - 0x514
    mode: ReadWrite<u32, Mode::Register>,
    /// Packet configuration register 0
    /// - Address 0x514 - 0x518
    pcnf0: ReadWrite<u32, PacketConfiguration0::Register>,
    /// Packet configuration register 1
    /// - Address: 0x518 - 0x51c
    pcnf1: ReadWrite<u32, PacketConfiguration1::Register>,
    /// Base address 0
    /// - Address: 0x51c - 0x520
    base0: ReadWrite<u32, BaseAddress::Register>,
    /// Base address 1
    /// - Address: 0x520 - 0x524
    base1: ReadWrite<u32, BaseAddress::Register>,
    /// Prefix bytes for logical addresses 0-3
    /// - Address: 0x524 - 0x528
    prefix0: ReadWrite<u32, Prefix0::Register>,
    /// Prefix bytes for logical addresses 4-7
    /// - Address: 0x528 - 0x52c
    prefix1: ReadWrite<u32, Prefix1::Register>,
    /// Transmit address select
    /// - Address: 0x52c - 0x530
    txaddress: ReadWrite<u32, TransmitAddress::Register>,
    /// Receive address select
    /// - Address: 0x530 - 0x534
    rxaddresses: ReadWrite<u32, ReceiveAddresses::Register>,
    /// CRC configuration
    /// - Address: 0x534 - 0x538
    crccnf: ReadWrite<u32, CrcConfiguration::Register>,
    /// CRC polynomial
    /// - Address: 0x538 - 0x53c
    crcpoly: ReadWrite<u32, CrcPolynomial::Register>,
    /// CRC initial value
    /// - Address: 0x53c - 0x540
    crcinit: ReadWrite<u32, CrcInitialValue::Register>,
    /// Reserved
    _reserved11: [u32; 1],
    /// Interframe spacing in microseconds
    /// - Address: 0x544 - 0x548
    tifs: ReadWrite<u32, InterFrameSpacing::Register>,
    /// RSSI sample
    /// - Address: 0x548 - 0x54c
    rssisample: ReadWrite<u32, RssiSample::Register>,
    /// Reserved
    _reserved12: [u32; 1],
    /// Current radio state
    /// - Address: 0x550 - 0x554
    state: ReadOnly<u32, State::Register>,
    /// Data whitening initial value
    /// - Address: 0x554 - 0x558
    datawhiteiv: ReadWrite<u32, DataWhiteIv::Register>,
    /// Reserved
    _reserved13: [u32; 2],
    /// Bit counter compare
    /// - Address: 0x560 - 0x564
    bcc: ReadWrite<u32, BitCounterCompare::Register>,
    /// Reserved
    _reserved14: [u32; 39],
    /// Device address base segments
    /// - Address: 0x600 - 0x620
    dab: [ReadWrite<u32, DeviceAddressBase::Register>; 8],
    /// Device address prefix
    /// - Address: 0x620 - 0x640
    dap: [ReadWrite<u32, DeviceAddressPrefix::Register>; 8],
    /// Device address match configuration
    /// - Address: 0x640 - 0x644
    dacnf: ReadWrite<u32, DeviceAddressMatch::Register>,
    /// MAC header Search Pattern Configuration
    /// - Address: 0x644 - 0x648
    mhrmatchconf: ReadWrite<u32, MACHeaderSearch::Register>,
    /// MAC Header Search Pattern Mask
    /// - Address: 0x648 - 0x64C
    mhrmatchmas: ReadWrite<u32, MACHeaderMask::Register>,
    /// Reserved
    _reserved15: [u32; 1],
    /// Radio mode configuration register
    /// - Address: 0x650 - 0x654
    modecnf0: ReadWrite<u32, RadioModeConfig::Register>,
    /// Reserved
    _reserved16: [u32; 6],
    /// Clear Channel Assesment (CCA) control register
    /// - Address: 0x66C - 0x670
    ccactrl: ReadWrite<u32, CCAControl::Register>,
    /// Reserved
    _reserved17: [u32; 611],
    /// Peripheral power control
    /// - Address: 0xFFC - 0x1000
    power: ReadWrite<u32, Task::Register>,
}

register_bitfields! [u32,
    /// Task register
    Task [
        /// Enable task
        ENABLE OFFSET(0) NUMBITS(1)
    ],
    /// Event register
    Event [
        /// Ready event
        READY OFFSET(0) NUMBITS(1)
    ],
    /// Shortcut register
    Shortcut [
        /// Shortcut between READY event and START task
        READY_START OFFSET(0) NUMBITS(1),
        /// Shortcut between END event and DISABLE task
        END_DISABLE OFFSET(1) NUMBITS(1),
        /// Shortcut between DISABLED event and TXEN task
        DISABLED_TXEN OFFSET(2) NUMBITS(1),
        /// Shortcut between DISABLED event and RXEN task
        DISABLED_RXEN OFFSET(3) NUMBITS(1),
        /// Shortcut between ADDRESS event and RSSISTART task
        ADDRESS_RSSISTART OFFSET(4) NUMBITS(1),
        /// Shortcut between END event and START task
        END_START OFFSET(5) NUMBITS(1),
        /// Shortcut between ADDRESS event and BCSTART task
        ADDRESS_BCSTART OFFSET(6) NUMBITS(1),
        /// Shortcut between DISABLED event and RSSISTOP task
        DISABLED_RSSISTOP OFFSET(8) NUMBITS(1)
    ],
    /// Interrupt register
    Interrupt [
        /// READY event
        READY OFFSET(0) NUMBITS(1),
        /// ADDRESS event
        ADDRESS OFFSET(1) NUMBITS(1),
        /// PAYLOAD event
        PAYLOAD OFFSET(2) NUMBITS(1),
        /// END event
        END OFFSET(3) NUMBITS(1),
        /// DISABLED event
        DISABLED OFFSET(4) NUMBITS(1),
        /// DEVMATCH event
        DEVMATCH OFFSET(5) NUMBITS(1),
        /// DEVMISS event
        DEVMISS OFFSET(6) NUMBITS(1),
        /// RSSIEND event
        RSSIEND OFFSET(7) NUMBITS(1),
        /// BCMATCH event
        BCMATCH OFFSET(10) NUMBITS(1),
        /// CRCOK event
        CRCOK OFFSET(12) NUMBITS(1),
        /// CRCERROR event
        CRCERROR OFFSET(13) NUMBITS(1),
        /// CCAIDLE event
        FRAMESTART OFFSET(14) NUMBITS(1),
        /// CCAIDLE event
        CCAIDLE OFFSET(17) NUMBITS(1),
        /// CCABUSY event
        CCABUSY OFFSET(18) NUMBITS(1)
    ],
    /// Receive match register
    ReceiveMatch [
        /// Logical address of which previous packet was received
        MATCH OFFSET(0) NUMBITS(3)
    ],
    /// Received CRC register
    ReceiveCrc [
        /// CRC field of previously received packet
        CRC OFFSET(0) NUMBITS(24)
    ],
    /// Device address match index register
    DeviceAddressIndex [
        /// Device address match index
        /// Index (n) of device address, see DAB\[n\] and DAP\[n\], that got an
        /// address match
        INDEX OFFSET(0) NUMBITS(3)
    ],
    /// Packet pointer register
    PacketPointer [
        /// Packet address to be used for the next transmission or reception. When transmitting, the packet pointed to by this
        /// address will be transmitted and when receiving, the received packet will be written to this address. This address is a byte
        /// aligned ram address.
        POINTER OFFSET(0) NUMBITS(32)
    ],
    /// Frequency register
    Frequency [
        /// Radio channel frequency
        /// Frequency = 2400 + FREQUENCY (MHz)
        FREQUENCY OFFSET(0) NUMBITS(7) [],
        /// Channel map selection.
        /// Channel map between 2400 MHZ .. 2500 MHZ
        MAP OFFSET(8) NUMBITS(1) [
            DEFAULT = 0,
            LOW = 1
        ]
    ],
    /// Transmitting power register
    TransmitPower [
        /// Radio output power
        POWER OFFSET(0) NUMBITS(8) [
            POS4DBM = 4,
            POS3DBM = 3,
            ODBM = 0,
            NEG4DBM = 0xfc,
            NEG8DBM = 0xf8,
            NEG12DBM = 0xf4,
            NEG16DBM = 0xf0,
            NEG20DBM = 0xec,
            NEG40DBM = 0xd8
        ]
    ],
    /// Data rate and modulation register
    Mode [
        /// Radio data rate and modulation setting.
        /// The radio supports Frequency-shift Keying (FSK) modulation
        MODE OFFSET(0) NUMBITS(4) [
            NRF_1MBIT = 0,
            NRF_2MBIT = 1,
            NRF_250KBIT = 2,
            BLE_1MBIT = 3,
            BLE_2MBIT = 4,
            BLE_LR125KBIT = 5,
            BLE_LR500KBIT = 6,
            IEEE802154_250KBIT = 15
        ]
    ],
    /// Packet configuration register 0
    PacketConfiguration0 [
        /// Length on air of LENGTH field in number of bits
        LFLEN OFFSET(0) NUMBITS(4) [],
        /// Length on air of S0 field in number of bytes
        S0LEN OFFSET(8) NUMBITS(1) [],
        /// Length on air of S1 field in number of bits.
        S1LEN OFFSET(16) NUMBITS(4) [],
        /// Include or exclude S1 field in RAM
        S1INCL OFFSET(20) NUMBITS(1) [
            AUTOMATIC = 0,
            INCLUDE = 1
        ],
        /// Length of preamble on air. Decision point: TASKS_START task
        PLEN OFFSET(24) NUMBITS(2) [
            EIGHT = 0,
            SIXTEEN = 1,
            THIRTYTWOZEROS = 2,
            LONGRANGE = 3
        ],
        CRCINC OFFSET(26) NUMBITS(1) [
            EXCLUDE = 0,
            INCLUDE = 1
        ]
    ],
    /// Packet configuration register 1
    PacketConfiguration1 [
        /// Maximum length of packet payload
        MAXLEN OFFSET(0) NUMBITS(8) [],
        /// Static length in number of bytes
        STATLEN OFFSET(8) NUMBITS(8) [],
        /// Base address length in number of bytes
        BALEN OFFSET(16) NUMBITS(3) [],
        /// On air endianness
        ENDIAN OFFSET(24) NUMBITS(1) [
            LITTLE = 0,
            BIG = 1
        ],
        /// Enable or disable packet whitening
        WHITEEN OFFSET(25) NUMBITS(1) [
            DISABLED = 0,
            ENABLED = 1
        ]
    ],
    /// Radio base address register
    BaseAddress [
        /// BASE0 or BASE1
        BASE OFFSET(0) NUMBITS(32)
    ],
    /// Radio prefix0 registers
    Prefix0 [
        /// Address prefix 0
        AP0 OFFSET(0) NUMBITS(8),
        /// Address prefix 1
        AP1 OFFSET(8) NUMBITS(8),
        /// Address prefix 2
        AP2 OFFSET(16) NUMBITS(8),
        /// Address prefix 3
        AP3 OFFSET(24) NUMBITS(8)
    ],
    /// Radio prefix0 registers
    Prefix1 [
        /// Address prefix 4
        AP4 OFFSET(0) NUMBITS(8),
        /// Address prefix 5
        AP5 OFFSET(8) NUMBITS(8),
        /// Address prefix 6
        AP6 OFFSET(16) NUMBITS(8),
        /// Address prefix 7
        AP7 OFFSET(24) NUMBITS(8)
    ],
    /// Transmit address register
    TransmitAddress [
        /// Logical address to be used when transmitting a packet
        ADDRESS OFFSET(0) NUMBITS(3)
    ],
    /// Receive addresses register
    ReceiveAddresses [
        /// Enable or disable reception on logical address 0-7
        ADDRESS OFFSET(0) NUMBITS(8)
    ],
    /// CRC configuration register
    CrcConfiguration [
        /// CRC length in bytes
        LEN OFFSET(0) NUMBITS(2) [
            DISABLED = 0,
            ONE = 1,
            TWO = 2,
            THREE = 3
        ],
        /// Include or exclude packet field from CRC calculation
        SKIPADDR OFFSET(8) NUMBITS(2) [
            INCLUDE = 0,
            EXCLUDE = 1,
            IEEE802154 = 2
        ]
    ],
    /// CRC polynomial register
    CrcPolynomial [
        /// CRC polynomial
        CRCPOLY OFFSET(0) NUMBITS(24)
    ],
    /// CRC initial value register
    CrcInitialValue [
       /// Initial value for CRC calculation
       CRCINIT OFFSET(0) NUMBITS(24)
    ],
    /// Inter Frame Spacing in us register
    InterFrameSpacing [
        /// Inter Frame Spacing in us
        /// Inter frame space is the time interval between two consecutive packets. It is defined as the time, in micro seconds, from the
        /// end of the last bit of the previous packet to the start of the first bit of the subsequent packet
        TIFS OFFSET(0) NUMBITS(8)
    ],
    /// RSSI sample register
    RssiSample [
        /// RSSI sample result
        RSSISAMPLE OFFSET(0) NUMBITS(7)
    ],
    /// Radio state register
    State [
        /// Current radio state
        STATE OFFSET(0) NUMBITS(4) [
            DISABLED = 0,
            RXRU = 1,
            RXIDLE = 2,
            RX = 3,
            RXDISABLED = 4,
            TXRU = 9,
            TXIDLE = 10,
            TX = 11,
            TXDISABLED = 12
        ]
    ],
    /// Data whitening initial value register
    DataWhiteIv [
        /// Data whitening initial value. Bit 6 is hard-wired to '1', writing '0'
        /// to it has no effect, and it will always be read back and used by the device as '1'
        DATEWHITEIV OFFSET(0) NUMBITS(7)
    ],
    /// Bit counter compare register
    BitCounterCompare [
        /// Bit counter compare
        BCC OFFSET(0) NUMBITS(32)
    ],
    /// Device address base register
    DeviceAddressBase [
        /// Device address base 0-7
        DAB OFFSET(0) NUMBITS(32)
    ],
    /// Device address prefix register
    DeviceAddressPrefix [
        /// Device address prefix 0-7
        DAP OFFSET(0) NUMBITS(32)
    ],
    /// Device address match configuration register
    DeviceAddressMatch [
        /// Enable or disable device address matching on 0-7
        ENA OFFSET(0) NUMBITS(8),
        /// TxAdd for device address 0-7
        TXADD OFFSET(8) NUMBITS(8)
    ],
    MACHeaderSearch [
        CONFIG OFFSET(0) NUMBITS(32)
    ],
    MACHeaderMask [
        PATTERN OFFSET(0) NUMBITS(32)
    ],
    CCAControl [
        CCAMODE OFFSET(0) NUMBITS(3) [
            ED_MODE = 0,
            CARRIER_MODE = 1,
            CARRIER_AND_ED_MODE = 2,
            CARRIER_OR_ED_MODE = 3,
            ED_MODE_TEST_1 = 4
        ],
        CCAEDTHRESH OFFSET(8) NUMBITS(8) [],
        CCACORRTHRESH OFFSET(16) NUMBITS(8) [],
        CCACORRCNT OFFSET(24) NUMBITS(8) []
    ],
    /// Radio mode configuration register
    RadioModeConfig [
        /// Radio ramp-up time
        RU OFFSET(0) NUMBITS(1) [
            DEFAULT = 0,
            FAST = 1
        ],
        /// Default TX value
        /// Specifies what the RADIO will transmit when it is not started, i.e. between:
        /// RADIO.EVENTS_READY and RADIO.TASKS_START
        /// RADIO.EVENTS_END and RADIO.TASKS_START
        DTX OFFSET(8) NUMBITS(2) [
            B1 = 0,
            B0 = 1,
            CENTER = 2
        ]
    ]
];

pub struct Radio<'p> {
    registers: StaticRef<RadioRegisters>,
    tx_power: Cell<TxPower>,
    rx_client: OptionalCell<&'static dyn radio::RxClient>,
    tx_client: OptionalCell<&'static dyn radio::TxClient>,
    tx_buf: TakeCell<'static, [u8]>,
    rx_buf: TakeCell<'static, [u8]>,
    addr: Cell<u16>,
    addr_long: Cell<[u8; 8]>,
    pan: Cell<u16>,
    cca_count: Cell<u8>,
    cca_be: Cell<u8>,
    random_nonce: Cell<u32>,
    channel: Cell<RadioChannel>,
    transmitting: Cell<bool>,
    timer0: OptionalCell<&'p crate::timer::TimerAlarm<'p>>,
}

impl<'a> AlarmClient for Radio<'a> {
    fn alarm(&self) {
        self.rx();
    }
}

impl<'p> Radio<'p> {
    pub const fn new() -> Self {
        Self {
            registers: RADIO_BASE,
            tx_power: Cell::new(TxPower::ZerodBm),
            rx_client: OptionalCell::empty(),
            tx_client: OptionalCell::empty(),
            tx_buf: TakeCell::empty(),
            rx_buf: TakeCell::empty(),
            addr: Cell::new(0),
            addr_long: Cell::new([0x00; 8]),
            pan: Cell::new(0),
            cca_count: Cell::new(0),
            cca_be: Cell::new(0),
            random_nonce: Cell::new(0xDEADBEEF),
            channel: Cell::new(RadioChannel::DataChannel26),
            transmitting: Cell::new(false),
            timer0: OptionalCell::empty(),
        }
    }

    pub fn set_timer_ref(&self, timer: &'p crate::timer::TimerAlarm<'p>) {
        self.timer0.set(timer);
    }

    pub fn is_enabled(&self) -> bool {
        self.registers
            .mode
            .matches_all(Mode::MODE::IEEE802154_250KBIT)
    }

    fn rx(&self) {
        self.registers.event_ready.write(Event::READY::CLEAR);

        if self.transmitting.get() {
            let tbuf = self.tx_buf.take().unwrap(); // Unwrap fail = Radio TX Buffer produced an invalid result when setting the DMA pointer.

            self.tx_buf.replace(self.set_dma_ptr(tbuf));
        } else {
            let rbuf = self.rx_buf.take().unwrap(); // Unwrap fail = Radio RX Buffer produced an invalid result when setting the DMA pointer.
            self.rx_buf.replace(self.set_dma_ptr(rbuf));
        }

        self.registers.task_rxen.write(Task::ENABLE::SET);

        self.enable_interrupts();
    }

    fn set_rx_address(&self) {
        self.registers
            .rxaddresses
            .write(ReceiveAddresses::ADDRESS.val(1));
    }

    fn set_tx_address(&self) {
        self.registers
            .txaddress
            .write(TransmitAddress::ADDRESS.val(0));
    }

    fn radio_on(&self) {
        // reset and enable power
        self.registers.power.write(Task::ENABLE::CLEAR);
        self.registers.power.write(Task::ENABLE::SET);
    }

    fn radio_off(&self) {
        self.registers.power.write(Task::ENABLE::CLEAR);
    }

    fn set_tx_power(&self) {
        self.registers.txpower.set(self.tx_power.get() as u32);
    }

    fn set_dma_ptr(&self, buffer: &'static mut [u8]) -> &'static mut [u8] {
        self.registers
            .packetptr
            .set(buffer.as_ptr() as u32 + MIMIC_PSDU_OFFSET);
        buffer
    }

    // TODO: Theres an additional step for 802154 rx/tx handling
    #[inline(never)]
    pub fn handle_interrupt(&self) {
        self.disable_all_interrupts();

        if self.registers.event_ready.is_set(Event::READY) {
            self.registers.event_ready.write(Event::READY::CLEAR);
            self.registers.event_end.write(Event::READY::CLEAR);
            if self.transmitting.get()
                && self.registers.state.get() == nrf5x::constants::RADIO_STATE_RXIDLE
            {
                self.registers.task_ccastart.write(Task::ENABLE::SET);
            } else {
                self.registers.task_start.write(Task::ENABLE::SET);
            }
        }

        if self.registers.event_framestart.is_set(Event::READY) {
            self.registers.event_framestart.write(Event::READY::CLEAR);
        }

        //   IF we receive the go ahead (channel is clear)
        // THEN start the transmit part of the radio
        if self.registers.event_ccaidle.is_set(Event::READY) {
            self.registers.event_ccaidle.write(Event::READY::CLEAR);
            self.registers.task_txen.write(Task::ENABLE::SET)
        }

        if self.registers.event_ccabusy.is_set(Event::READY) {
            self.registers.event_ccabusy.write(Event::READY::CLEAR);
            self.registers.event_ready.write(Event::READY::CLEAR);
            self.registers.task_disable.write(Task::ENABLE::SET);
            while self.registers.event_disabled.get() == 0 {}
            self.registers.event_disabled.write(Event::READY::CLEAR);
            //need to back off for a period of time outlined
            //in the IEEE 802.15.4 standard (see Figure 69 in
            //section 7.5.1.4 The CSMA-CA algorithm of the
            //standard).
            if self.cca_count.get() < IEEE802154_MAX_POLLING_ATTEMPTS {
                self.cca_count.set(self.cca_count.get() + 1);
                self.cca_be.set(self.cca_be.get() + 1);
                let backoff_periods = self.random_nonce() & ((1 << self.cca_be.get()) - 1);
                self.timer0
                    .unwrap_or_panic() // Unwrap fail = Missing timer reference for CSMA
                    .set_alarm(
                        kernel::hil::time::Ticks32::from(0),
                        kernel::hil::time::Ticks32::from(
                            backoff_periods * (IEEE802154_BACKOFF_PERIOD as u32),
                        ),
                    );
            } else {
                self.transmitting.set(false);
                //if we are transmitting, the CRCstatus check is always going to be an error
                let result = Err(ErrorCode::BUSY);
                //TODO: Acked is flagged as false until I get around to fixing it.
                self.tx_client.map(|client| {
                    let tbuf = self.tx_buf.take().unwrap(); // Unwrap fail = TX Buffer produced error when sending it back to the requestor after the channel was busy.
                    client.send_done(tbuf, false, result)
                });
            }

            self.enable_interrupts();
        }

        // tx or rx finished!
        if self.registers.event_end.is_set(Event::READY) {
            self.registers.event_end.write(Event::READY::CLEAR);

            let result = if self.registers.crcstatus.is_set(Event::READY) {
                Ok(())
            } else {
                Err(ErrorCode::FAIL)
            };

            match self.registers.state.get() {
                nrf5x::constants::RADIO_STATE_TXRU
                | nrf5x::constants::RADIO_STATE_TXIDLE
                | nrf5x::constants::RADIO_STATE_TXDISABLE
                | nrf5x::constants::RADIO_STATE_TX => {
                    self.transmitting.set(false);
                    //if we are transmitting, the CRCstatus check is always going to be an error
                    let result = Ok(());
                    //TODO: Acked is flagged as false until I get around to fixing it.
                    self.tx_client.map(|client| {
                        let tbuf = self.tx_buf.take().unwrap(); // Unwrap fail = TX Buffer produced error when sending it back to the requestor after successful transmission.

                        client.send_done(tbuf, false, result)
                    });
                }
                nrf5x::constants::RADIO_STATE_RXRU
                | nrf5x::constants::RADIO_STATE_RXIDLE
                | nrf5x::constants::RADIO_STATE_RXDISABLE
                | nrf5x::constants::RADIO_STATE_RX => {
                    self.rx_client.map(|client| {
                        let rbuf = self.rx_buf.take().unwrap(); // Unwrap fail = RX Buffer produced error when sending received packet to requestor

                        let frame_len = rbuf[MIMIC_PSDU_OFFSET as usize] as usize - radio::MFR_SIZE;
                        // Length is: S0 (0 Byte) + Length (1 Byte) + S1 (0 Bytes) + Payload
                        // And because the length field is directly read from the packet
                        // We need to add 2 to length to get the total length

                        client.receive(rbuf, frame_len, self.registers.crcstatus.get() == 1, result)
                    });
                }
                // Radio state - Disabled
                _ => (),
            }
            self.radio_off();
            self.radio_initialize();
            self.rx();
        }
        self.enable_interrupts();
    }

    pub fn enable_interrupts(&self) {
        self.registers.intenset.write(
            Interrupt::READY::SET
                + Interrupt::CCAIDLE::SET
                + Interrupt::CCABUSY::SET
                + Interrupt::END::SET
                + Interrupt::FRAMESTART::SET,
        );
    }

    pub fn enable_interrupt(&self, intr: u32) {
        self.registers.intenset.set(intr);
    }

    pub fn clear_interrupt(&self, intr: u32) {
        self.registers.intenclr.set(intr);
    }

    pub fn disable_all_interrupts(&self) {
        // disable all possible interrupts
        self.registers.intenclr.set(0xffffffff);
    }

    fn radio_initialize(&self) {
        self.radio_on();

        // Radio disable
        self.registers.event_disabled.set(0);
        self.registers.task_disable.write(Task::ENABLE::SET);
        while self.registers.event_disabled.get() == 0 {}
        // end radio disable

        self.ieee802154_set_channel_rate();

        self.ieee802154_set_packet_config();

        self.ieee802154_set_crc_config();

        self.ieee802154_set_rampup_mode();

        self.ieee802154_set_cca_config();

        self.ieee802154_set_tx_power();

        self.ieee802154_set_channel_freq(self.channel.get());

        self.set_tx_address();
        self.set_rx_address();

        // First step in transmitting or receiving is entering rx mode
        self.rx();
    }

    // IEEE802.15.4 SPECIFICATION Section 6.20.12.5 of the NRF52840 Datasheet
    fn ieee802154_set_crc_config(&self) {
        self.registers
            .crccnf
            .write(CrcConfiguration::LEN::TWO + CrcConfiguration::SKIPADDR::IEEE802154);
        self.registers
            .crcinit
            .set(nrf5x::constants::RADIO_CRCINIT_IEEE802154);
        self.registers
            .crcpoly
            .set(nrf5x::constants::RADIO_CRCPOLY_IEEE802154);
    }

    fn ieee802154_set_rampup_mode(&self) {
        self.registers
            .modecnf0
            .write(RadioModeConfig::RU::FAST + RadioModeConfig::DTX::CENTER);
    }

    fn ieee802154_set_cca_config(&self) {
        self.registers.ccactrl.write(
            CCAControl::CCAMODE.val(nrf5x::constants::IEEE802154_CCA_MODE)
                + CCAControl::CCAEDTHRESH.val(nrf5x::constants::IEEE802154_CCA_ED_THRESH)
                + CCAControl::CCACORRTHRESH.val(nrf5x::constants::IEEE802154_CCA_CORR_THRESH)
                + CCAControl::CCACORRCNT.val(nrf5x::constants::IEEE802154_CCA_CORR_CNT),
        );
    }

    // Packet configuration
    // Settings taken from RiotOS nrf52840 15.4 driver
    fn ieee802154_set_packet_config(&self) {
        self.registers.pcnf0.write(
            PacketConfiguration0::LFLEN.val(8)
                + PacketConfiguration0::PLEN::THIRTYTWOZEROS
                + PacketConfiguration0::CRCINC::INCLUDE,
        );

        self.registers
            .pcnf1
            .write(PacketConfiguration1::MAXLEN.val(nrf5x::constants::RADIO_PAYLOAD_LENGTH as u32));
    }

    fn ieee802154_set_channel_rate(&self) {
        self.registers.mode.write(Mode::MODE::IEEE802154_250KBIT);
    }

    fn ieee802154_set_channel_freq(&self, channel: RadioChannel) {
        self.registers
            .frequency
            .write(Frequency::FREQUENCY.val(channel as u32));
    }

    fn ieee802154_set_tx_power(&self) {
        self.set_tx_power();
    }

    pub fn startup(&self) -> Result<(), ErrorCode> {
        self.radio_initialize();
        Ok(())
    }

    // Returns a new pseudo-random number and updates the randomness state.
    //
    // Uses the [Xorshift](https://en.wikipedia.org/wiki/Xorshift) algorithm to
    // produce pseudo-random numbers. Uses the `random_nonce` field to keep
    // state.
    fn random_nonce(&self) -> u32 {
        let mut next_nonce = ::core::num::Wrapping(self.random_nonce.get());
        next_nonce ^= next_nonce << 13;
        next_nonce ^= next_nonce >> 17;
        next_nonce ^= next_nonce << 5;
        self.random_nonce.set(next_nonce.0);
        self.random_nonce.get()
    }
}

impl<'p> kernel::hil::radio::RadioConfig for Radio<'p> {
    fn initialize(
        &self,
        _spi_buf: &'static mut [u8],
        _reg_write: &'static mut [u8],
        _reg_read: &'static mut [u8],
    ) -> Result<(), ErrorCode> {
        self.radio_initialize();
        Ok(())
    }

    fn set_power_client(&self, _client: &'static dyn PowerClient) {
        //
    }

    fn reset(&self) -> Result<(), ErrorCode> {
        self.radio_on();
        Ok(())
    }
    fn start(&self) -> Result<(), ErrorCode> {
        let _ = self.reset();
        Ok(())
    }
    fn stop(&self) -> Result<(), ErrorCode> {
        self.radio_off();
        Ok(())
    }
    fn is_on(&self) -> bool {
        true
    }
    fn busy(&self) -> bool {
        false
    }

    //#################################################
    ///These methods are holdovers from when the radio HIL was mostly to an external
    ///module over an interface
    //#################################################

    //fn set_power_client(&self, client: &'static radio::PowerClient){

    //}
    /// Commit the config calls to hardware, changing the address,
    /// PAN ID, TX power, and channel to the specified values, issues
    /// a callback to the config client when done.
    fn config_commit(&self) {
        self.radio_off();
        self.radio_initialize();
    }

    fn set_config_client(&self, _client: &'static dyn radio::ConfigClient) {}

    //#################################################
    /// Accessors
    //#################################################

    fn get_address(&self) -> u16 {
        self.addr.get()
    }

    fn get_address_long(&self) -> [u8; 8] {
        self.addr_long.get()
    }

    /// The 16-bit PAN ID
    fn get_pan(&self) -> u16 {
        self.pan.get()
    }
    /// The transmit power, in dBm
    fn get_tx_power(&self) -> i8 {
        self.tx_power.get() as i8
    }
    /// The 802.15.4 channel
    fn get_channel(&self) -> u8 {
        self.channel.get().get_channel_index()
    }

    //#################################################
    /// Mutators
    //#################################################

    fn set_address(&self, addr: u16) {
        self.addr.set(addr);
    }

    fn set_address_long(&self, addr: [u8; 8]) {
        self.addr_long.set(addr);
    }

    fn set_pan(&self, id: u16) {
        self.pan.set(id);
    }

    fn set_channel(&self, chan: u8) -> Result<(), ErrorCode> {
        match RadioChannel::try_from(chan) {
            Err(_) => Err(ErrorCode::NOSUPPORT),
            Ok(res) => {
                self.channel.set(res);
                Ok(())
            }
        }
    }

    fn set_tx_power(&self, tx_power: i8) -> Result<(), ErrorCode> {
        // Convert u8 to TxPower
        match nrf5x::constants::TxPower::try_from(tx_power as u8) {
            // Invalid transmitting power, propogate error
            Err(_) => Err(ErrorCode::NOSUPPORT),
            // Valid transmitting power, propogate success
            Ok(res) => {
                self.tx_power.set(res);
                Ok(())
            }
        }
    }
}

impl<'p> kernel::hil::radio::RadioData for Radio<'p> {
    fn set_receive_client(&self, client: &'static dyn radio::RxClient, buffer: &'static mut [u8]) {
        self.rx_client.set(client);
        self.rx_buf.replace(buffer);
    }

    fn set_receive_buffer(&self, buffer: &'static mut [u8]) {
        self.rx_buf.replace(buffer);
    }

    fn set_transmit_client(&self, client: &'static dyn radio::TxClient) {
        self.tx_client.set(client);
    }

    fn transmit(
        &self,
        buf: &'static mut [u8],
        frame_len: usize,
    ) -> Result<(), (ErrorCode, &'static mut [u8])> {
        if self.tx_buf.is_some() || self.transmitting.get() {
            return Err((ErrorCode::BUSY, buf));
        } else if radio::PSDU_OFFSET + frame_len >= buf.len() {
            // Not enough room for CRC
            return Err((ErrorCode::SIZE, buf));
        }

        buf[MIMIC_PSDU_OFFSET as usize] = (frame_len + radio::MFR_SIZE) as u8;
        self.tx_buf.replace(buf);

        self.transmitting.set(true);

        self.cca_count.set(0);
        self.cca_be.set(IEEE802154_MIN_BE);

        self.radio_off();
        self.radio_initialize();
        Ok(())
    }
}

// Radio states as defined in the nrf52840dk's product spec
// section 6.20.5 Radio states
// The following states are the state 'S' in RadioStateMachine<S>
/// default state at radio startup
struct Disabled;
struct RxRu;
struct RxIdle;
struct Rx;
struct TxRu;
struct TxIdle;
struct Tx;
struct RxDisable;
struct TxDisable;

/// Radio state machine that encodes the Radio states described in
/// the nrf52840 product spec section '6.20.5 Radio states' as type S.
struct RadioStateMachine<'a, S> {
    radio: Radio<'a>,
    state: S,
}

// The Radio state machine starts in the 'DISABLED' state
impl<'a> RadioStateMachine<'a, Disabled> {
    /// A radio can only be created from the DISABLED state
    pub const fn new() -> Self {
        Self {
            radio: Radio::new(),
            state: Disabled,
        }
    }
    fn state_transition(self) -> RadioStateMachineWrapper<'a> {
        match self.radio.registers.state.get() {
            nrf5x::constants::RADIO_STATE_TXRU => RadioStateMachineWrapper::TxRu(self.into()),
            nrf5x::constants::RADIO_STATE_TXIDLE => {
                let txru_radio = RadioStateMachine::<TxRu>::from(self);
                RadioStateMachineWrapper::TxIdle(txru_radio.into())
            }
            nrf5x::constants::RADIO_STATE_TXDISABLE => {
                let txru_radio = RadioStateMachine::<TxRu>::from(self);
                RadioStateMachineWrapper::TxDisable(txru_radio.into())
            }
            nrf5x::constants::RADIO_STATE_TX => {
                let txru_radio = RadioStateMachine::<TxRu>::from(self);
                let txidle_radio = RadioStateMachine::<TxIdle>::from(txru_radio);
                RadioStateMachineWrapper::Tx(txidle_radio.into())
            }
            nrf5x::constants::RADIO_STATE_RXRU => RadioStateMachineWrapper::RxRu(self.into()),
            nrf5x::constants::RADIO_STATE_RXIDLE => {
                let rxru_radio = RadioStateMachine::<RxRu>::from(self);
                RadioStateMachineWrapper::RxIdle(rxru_radio.into())
            }
            nrf5x::constants::RADIO_STATE_RXDISABLE => {
                let rxru_radio = RadioStateMachine::<RxRu>::from(self);
                RadioStateMachineWrapper::RxDisable(rxru_radio.into())
            }
            nrf5x::constants::RADIO_STATE_RX => {
                let rxru_radio = RadioStateMachine::<RxRu>::from(self);
                let rxidle_radio = RadioStateMachine::<RxIdle>::from(rxru_radio);
                RadioStateMachineWrapper::Rx(rxidle_radio.into())
            }
            _ => RadioStateMachineWrapper::Disabled(self),
        }
    }
}

impl<'a> RadioStateMachine<'a, RxRu> {
    fn state_transition(self) -> RadioStateMachineWrapper<'a> {
        match self.radio.registers.state.get() {
            nrf5x::constants::RADIO_STATE_TXRU => {
                let rxdisable_radio = RadioStateMachine::<RxDisable>::from(self);
                let disabled_radio = RadioStateMachine::<Disabled>::from(rxdisable_radio);
                RadioStateMachineWrapper::TxRu(disabled_radio.into())
            }
            nrf5x::constants::RADIO_STATE_TXIDLE => {
                let rxdisable_radio = RadioStateMachine::<RxDisable>::from(self);
                let disabled_radio = RadioStateMachine::<Disabled>::from(rxdisable_radio);
                let txru_radio = RadioStateMachine::<TxRu>::from(disabled_radio);
                RadioStateMachineWrapper::TxIdle(txru_radio.into())
            }
            nrf5x::constants::RADIO_STATE_TXDISABLE => {
                let rxdisable_radio = RadioStateMachine::<RxDisable>::from(self);
                let disabled_radio = RadioStateMachine::<Disabled>::from(rxdisable_radio);
                let txru_radio = RadioStateMachine::<TxRu>::from(disabled_radio);
                RadioStateMachineWrapper::TxDisable(txru_radio.into())
            }
            nrf5x::constants::RADIO_STATE_TX => {
                let rxdisable_radio = RadioStateMachine::<RxDisable>::from(self);
                let disabled_radio = RadioStateMachine::<Disabled>::from(rxdisable_radio);
                let txru_radio = RadioStateMachine::<TxRu>::from(disabled_radio);
                let txidle_radio = RadioStateMachine::<TxIdle>::from(txru_radio);
                RadioStateMachineWrapper::Tx(txidle_radio.into())
            }
            nrf5x::constants::RADIO_STATE_RXIDLE => RadioStateMachineWrapper::RxIdle(self.into()),
            nrf5x::constants::RADIO_STATE_RXDISABLE => {
                RadioStateMachineWrapper::RxDisable(self.into())
            }
            nrf5x::constants::RADIO_STATE_RX => {
                let rxidle_radio = RadioStateMachine::<RxIdle>::from(self);
                RadioStateMachineWrapper::Rx(rxidle_radio.into())
            }
            nrf5x::constants::RADIO_STATE_DISABLE => {
                let rxdisable_radio = RadioStateMachine::<RxDisable>::from(self);
                RadioStateMachineWrapper::Disabled(rxdisable_radio.into())
            }
            _ => RadioStateMachineWrapper::RxRu(self.into()),
        }
    }
}

impl<'a> RadioStateMachine<'a, RxIdle> {
    fn state_transition(self) -> RadioStateMachineWrapper<'a> {
        match self.radio.registers.state.get() {
            nrf5x::constants::RADIO_STATE_TXRU => RadioStateMachineWrapper::TxRu(self.into()),
            nrf5x::constants::RADIO_STATE_TXIDLE => {
                let txru_radio = RadioStateMachine::<TxRu>::from(self);
                RadioStateMachineWrapper::TxIdle(txru_radio.into())
            }
            nrf5x::constants::RADIO_STATE_TXDISABLE => {
                let txru_radio = RadioStateMachine::<TxRu>::from(self);
                RadioStateMachineWrapper::TxDisable(txru_radio.into())
            }
            nrf5x::constants::RADIO_STATE_TX => {
                let txru_radio = RadioStateMachine::<TxRu>::from(self);
                let txidle_radio = RadioStateMachine::<TxIdle>::from(txru_radio);
                RadioStateMachineWrapper::Tx(txidle_radio.into())
            }
            nrf5x::constants::RADIO_STATE_RXRU => RadioStateMachineWrapper::RxRu(self.into()),
            nrf5x::constants::RADIO_STATE_RXDISABLE => {
                RadioStateMachineWrapper::RxDisable(self.into())
            }
            nrf5x::constants::RADIO_STATE_RX => RadioStateMachineWrapper::Rx(self.into()),
            nrf5x::constants::RADIO_STATE_DISABLE => {
                let rxdisable_radio = RadioStateMachine::<RxDisable>::from(self);
                RadioStateMachineWrapper::Disabled(rxdisable_radio.into())
            }
            _ => RadioStateMachineWrapper::RxIdle(self.into()),
        }
    }
}

impl<'a> RadioStateMachine<'a, Rx> {
    fn state_transition(self) -> RadioStateMachineWrapper<'a> {
        match self.radio.registers.state.get() {
            nrf5x::constants::RADIO_STATE_TXRU => {
                let rxidle_radio = RadioStateMachine::<RxIdle>::from(self);
                RadioStateMachineWrapper::TxRu(rxidle_radio.into())
            }
            nrf5x::constants::RADIO_STATE_TXIDLE => {
                let rxidle_radio = RadioStateMachine::<RxIdle>::from(self);
                let txru_radio = RadioStateMachine::<TxRu>::from(rxidle_radio);
                RadioStateMachineWrapper::TxIdle(txru_radio.into())
            }
            nrf5x::constants::RADIO_STATE_TXDISABLE => {
                let rxidle_radio = RadioStateMachine::<RxIdle>::from(self);
                let txru_radio = RadioStateMachine::<TxRu>::from(rxidle_radio);
                RadioStateMachineWrapper::TxDisable(txru_radio.into())
            }
            nrf5x::constants::RADIO_STATE_TX => {
                let rxidle_radio = RadioStateMachine::<RxIdle>::from(self);
                let txru_radio = RadioStateMachine::<TxRu>::from(rxidle_radio);
                let txidle_radio = RadioStateMachine::<TxIdle>::from(txru_radio);
                RadioStateMachineWrapper::Tx(txidle_radio.into())
            }
            nrf5x::constants::RADIO_STATE_RXRU => {
                let rxidle_radio = RadioStateMachine::<RxIdle>::from(self);
                RadioStateMachineWrapper::RxRu(rxidle_radio.into())
            }
            nrf5x::constants::RADIO_STATE_RXIDLE => RadioStateMachineWrapper::RxIdle(self.into()),
            nrf5x::constants::RADIO_STATE_RXDISABLE => {
                RadioStateMachineWrapper::RxDisable(self.into())
            }
            nrf5x::constants::RADIO_STATE_DISABLE => {
                let rxdisable_radio = RadioStateMachine::<RxDisable>::from(self);
                RadioStateMachineWrapper::Disabled(rxdisable_radio.into())
            }
            _ => RadioStateMachineWrapper::Rx(self.into()),
        }
    }
}

impl<'a> RadioStateMachine<'a, TxRu> {
    fn state_transition(self) -> RadioStateMachineWrapper<'a> {
        match self.radio.registers.state.get() {
            nrf5x::constants::RADIO_STATE_TXIDLE => RadioStateMachineWrapper::TxIdle(self.into()),
            nrf5x::constants::RADIO_STATE_TXDISABLE => {
                RadioStateMachineWrapper::TxDisable(self.into())
            }
            nrf5x::constants::RADIO_STATE_TX => {
                let txidle_radio = RadioStateMachine::<TxIdle>::from(self);
                RadioStateMachineWrapper::Tx(txidle_radio.into())
            }
            nrf5x::constants::RADIO_STATE_RXRU => {
                let txidle_radio = RadioStateMachine::<TxIdle>::from(self);
                RadioStateMachineWrapper::RxRu(txidle_radio.into())
            }
            nrf5x::constants::RADIO_STATE_RXIDLE => {
                let txidle_radio = RadioStateMachine::<TxIdle>::from(self);
                let rxru_radio = RadioStateMachine::<RxRu>::from(txidle_radio);
                RadioStateMachineWrapper::RxIdle(rxru_radio.into())
            }
            nrf5x::constants::RADIO_STATE_RXDISABLE => {
                let txidle_radio = RadioStateMachine::<TxIdle>::from(self);
                let rxru_radio = RadioStateMachine::<RxRu>::from(txidle_radio);
                RadioStateMachineWrapper::RxDisable(rxru_radio.into())
            }
            nrf5x::constants::RADIO_STATE_RX => {
                let txidle_radio = RadioStateMachine::<TxIdle>::from(self);
                let rxru_radio = RadioStateMachine::<RxRu>::from(txidle_radio);
                let rxidle_radio = RadioStateMachine::<RxIdle>::from(rxru_radio);
                RadioStateMachineWrapper::Rx(rxidle_radio.into())
            }
            nrf5x::constants::RADIO_STATE_DISABLE => {
                let txdisable_radio = RadioStateMachine::<TxDisable>::from(self);
                RadioStateMachineWrapper::Disabled(txdisable_radio.into())
            }
            _ => RadioStateMachineWrapper::TxRu(self.into()),
        }
    }
}

impl<'a> RadioStateMachine<'a, TxIdle> {
    fn state_transition(self) -> RadioStateMachineWrapper<'a> {
        match self.radio.registers.state.get() {
            nrf5x::constants::RADIO_STATE_TXRU => RadioStateMachineWrapper::TxRu(self.into()),
            nrf5x::constants::RADIO_STATE_TXDISABLE => {
                RadioStateMachineWrapper::TxDisable(self.into())
            }
            nrf5x::constants::RADIO_STATE_TX => {
                let txidle_radio = RadioStateMachine::<TxIdle>::from(self);
                RadioStateMachineWrapper::Tx(txidle_radio.into())
            }
            nrf5x::constants::RADIO_STATE_RXRU => RadioStateMachineWrapper::RxRu(self.into()),
            nrf5x::constants::RADIO_STATE_RXIDLE => {
                let rxru_radio = RadioStateMachine::<RxRu>::from(self);
                RadioStateMachineWrapper::RxIdle(rxru_radio.into())
            }
            nrf5x::constants::RADIO_STATE_RXDISABLE => {
                let rxru_radio = RadioStateMachine::<RxRu>::from(self);
                RadioStateMachineWrapper::RxDisable(rxru_radio.into())
            }
            nrf5x::constants::RADIO_STATE_RX => {
                let rxru_radio = RadioStateMachine::<RxRu>::from(self);
                let rxidle_radio = RadioStateMachine::<RxIdle>::from(rxru_radio);
                RadioStateMachineWrapper::Rx(rxidle_radio.into())
            }
            nrf5x::constants::RADIO_STATE_DISABLE => {
                let txdisable_radio = RadioStateMachine::<TxDisable>::from(self);
                RadioStateMachineWrapper::Disabled(txdisable_radio.into())
            }
            _ => RadioStateMachineWrapper::TxIdle(self.into()),
        }
    }
}

impl<'a> RadioStateMachine<'a, Tx> {
    fn state_transition(self) -> RadioStateMachineWrapper<'a> {
        match self.radio.registers.state.get() {
            nrf5x::constants::RADIO_STATE_TXRU => {
                let txidle_radio = RadioStateMachine::<TxIdle>::from(self);
                RadioStateMachineWrapper::TxRu(txidle_radio.into())
            }
            nrf5x::constants::RADIO_STATE_TXIDLE => RadioStateMachineWrapper::TxIdle(self.into()),
            nrf5x::constants::RADIO_STATE_TXDISABLE => {
                RadioStateMachineWrapper::TxDisable(self.into())
            }
            nrf5x::constants::RADIO_STATE_RXRU => {
                let txidle_radio = RadioStateMachine::<TxIdle>::from(self);
                RadioStateMachineWrapper::RxRu(txidle_radio.into())
            }
            nrf5x::constants::RADIO_STATE_RXIDLE => {
                let txidle_radio = RadioStateMachine::<TxIdle>::from(self);
                let rxru_radio = RadioStateMachine::<RxRu>::from(txidle_radio);
                RadioStateMachineWrapper::RxIdle(rxru_radio.into())
            }
            nrf5x::constants::RADIO_STATE_RXDISABLE => {
                let txidle_radio = RadioStateMachine::<TxIdle>::from(self);
                let rxru_radio = RadioStateMachine::<RxRu>::from(txidle_radio);
                RadioStateMachineWrapper::RxDisable(rxru_radio.into())
            }
            nrf5x::constants::RADIO_STATE_RX => {
                let txidle_radio = RadioStateMachine::<TxIdle>::from(self);
                let rxru_radio = RadioStateMachine::<RxRu>::from(txidle_radio);
                // The following violates Rust's ownership rules
                // txidle_radio.radio.rx();
                let rxidle_radio = RadioStateMachine::<RxIdle>::from(rxru_radio);
                RadioStateMachineWrapper::Rx(rxidle_radio.into())
            }
            nrf5x::constants::RADIO_STATE_DISABLE => {
                // The following is an example of an invalid state transition that is caught at compile time.
                // let disabled_radio = RadioStateMachine::<Disabled>::from(tx_radio);
                let txdisable_radio = RadioStateMachine::<TxDisable>::from(self);
                RadioStateMachineWrapper::Disabled(txdisable_radio.into())
            }
            _ => RadioStateMachineWrapper::Tx(self.into()),
        }
    }
}

impl<'a> RadioStateMachine<'a, RxDisable> {
    fn state_transition(self) -> RadioStateMachineWrapper<'a> {
        match self.radio.registers.state.get() {
            nrf5x::constants::RADIO_STATE_TXRU => {
                let disabled_radio = RadioStateMachine::<Disabled>::from(self);
                RadioStateMachineWrapper::TxRu(disabled_radio.into())
            }
            nrf5x::constants::RADIO_STATE_TXIDLE => {
                let disabled_radio = RadioStateMachine::<Disabled>::from(self);
                let txru_radio = RadioStateMachine::<TxRu>::from(disabled_radio);
                RadioStateMachineWrapper::TxIdle(txru_radio.into())
            }
            nrf5x::constants::RADIO_STATE_TXDISABLE => {
                let disabled_radio = RadioStateMachine::<Disabled>::from(self);
                let txru_radio = RadioStateMachine::<TxRu>::from(disabled_radio);
                RadioStateMachineWrapper::TxDisable(txru_radio.into())
            }
            nrf5x::constants::RADIO_STATE_TX => {
                let disabled_radio = RadioStateMachine::<Disabled>::from(self);
                let txru_radio = RadioStateMachine::<TxRu>::from(disabled_radio);
                let txidle_radio = RadioStateMachine::<TxIdle>::from(txru_radio);
                RadioStateMachineWrapper::Tx(txidle_radio.into())
            }
            nrf5x::constants::RADIO_STATE_RXRU => {
                let disabled_radio = RadioStateMachine::<Disabled>::from(self);
                RadioStateMachineWrapper::RxRu(disabled_radio.into())
            }
            nrf5x::constants::RADIO_STATE_RXIDLE => {
                let disabled_radio = RadioStateMachine::<Disabled>::from(self);
                let rxru_radio = RadioStateMachine::<RxRu>::from(disabled_radio);
                RadioStateMachineWrapper::RxIdle(rxru_radio.into())
            }
            nrf5x::constants::RADIO_STATE_RX => {
                let disabled_radio = RadioStateMachine::<Disabled>::from(self);
                let rxru_radio = RadioStateMachine::<RxRu>::from(disabled_radio);
                let rxidle_radio = RadioStateMachine::<RxIdle>::from(rxru_radio);
                RadioStateMachineWrapper::Rx(rxidle_radio.into())
            }
            nrf5x::constants::RADIO_STATE_DISABLE => {
                RadioStateMachineWrapper::Disabled(self.into())
            }
            _ => RadioStateMachineWrapper::RxDisable(self.into()),
        }
    }
}

impl<'a> RadioStateMachine<'a, TxDisable> {
    fn state_transition(self) -> RadioStateMachineWrapper<'a> {
        match self.radio.registers.state.get() {
            nrf5x::constants::RADIO_STATE_TXRU => {
                let disabled_radio = RadioStateMachine::<Disabled>::from(self);
                RadioStateMachineWrapper::TxRu(disabled_radio.into())
            }
            nrf5x::constants::RADIO_STATE_TXIDLE => {
                let disabled_radio = RadioStateMachine::<Disabled>::from(self);
                let txru_radio = RadioStateMachine::<TxRu>::from(disabled_radio);
                RadioStateMachineWrapper::TxIdle(txru_radio.into())
            }
            nrf5x::constants::RADIO_STATE_TX => {
                let disabled_radio = RadioStateMachine::<Disabled>::from(self);
                let txru_radio = RadioStateMachine::<TxRu>::from(disabled_radio);
                let txidle_radio = RadioStateMachine::<TxIdle>::from(txru_radio);
                RadioStateMachineWrapper::Tx(txidle_radio.into())
            }
            nrf5x::constants::RADIO_STATE_RXRU => {
                let disabled_radio = RadioStateMachine::<Disabled>::from(self);
                RadioStateMachineWrapper::RxRu(disabled_radio.into())
            }
            nrf5x::constants::RADIO_STATE_RXIDLE => {
                let disabled_radio = RadioStateMachine::<Disabled>::from(self);
                let rxru_radio = RadioStateMachine::<RxRu>::from(disabled_radio);
                RadioStateMachineWrapper::RxIdle(rxru_radio.into())
            }
            nrf5x::constants::RADIO_STATE_RXDISABLE => {
                let disabled_radio = RadioStateMachine::<Disabled>::from(self);
                let rxru_radio = RadioStateMachine::<RxRu>::from(disabled_radio);
                RadioStateMachineWrapper::RxDisable(rxru_radio.into())
            }
            nrf5x::constants::RADIO_STATE_RX => {
                let disabled_radio = RadioStateMachine::<Disabled>::from(self);
                let rxru_radio = RadioStateMachine::<RxRu>::from(disabled_radio);
                let rxidle_radio = RadioStateMachine::<RxIdle>::from(rxru_radio);
                RadioStateMachineWrapper::Rx(rxidle_radio.into())
            }
            nrf5x::constants::RADIO_STATE_DISABLE => {
                RadioStateMachineWrapper::Disabled(self.into())
            }
            _ => RadioStateMachineWrapper::TxDisable(self.into()),
        }
    }
}

// We implement the From trait for valid state transitions only
// RxDisable, TxDisable --> Disabled
impl<'a> From<RadioStateMachine<'a, RxDisable>> for RadioStateMachine<'a, Disabled> {
    fn from(r: RadioStateMachine<'a, RxDisable>) -> Self {
        RadioStateMachine {
            radio: r.radio,
            state: Disabled,
        }
    }
}
impl<'a> From<RadioStateMachine<'a, TxDisable>> for RadioStateMachine<'a, Disabled> {
    fn from(r: RadioStateMachine<'a, TxDisable>) -> Self {
        RadioStateMachine {
            radio: r.radio,
            state: Disabled,
        }
    }
}
// TxRu, TxIdle, Tx --> TxDisable
impl<'a> From<RadioStateMachine<'a, TxRu>> for RadioStateMachine<'a, TxDisable> {
    fn from(r: RadioStateMachine<'a, TxRu>) -> Self {
        RadioStateMachine {
            radio: r.radio,
            state: TxDisable,
        }
    }
}
impl<'a> From<RadioStateMachine<'a, TxIdle>> for RadioStateMachine<'a, TxDisable> {
    fn from(r: RadioStateMachine<'a, TxIdle>) -> Self {
        RadioStateMachine {
            radio: r.radio,
            state: TxDisable,
        }
    }
}
impl<'a> From<RadioStateMachine<'a, Tx>> for RadioStateMachine<'a, TxDisable> {
    fn from(r: RadioStateMachine<'a, Tx>) -> Self {
        RadioStateMachine {
            radio: r.radio,
            state: TxDisable,
        }
    }
}
// RxRu, RxIdle, Rx --> RxDisable
impl<'a> From<RadioStateMachine<'a, RxRu>> for RadioStateMachine<'a, RxDisable> {
    fn from(r: RadioStateMachine<'a, RxRu>) -> Self {
        RadioStateMachine {
            radio: r.radio,
            state: RxDisable,
        }
    }
}
impl<'a> From<RadioStateMachine<'a, RxIdle>> for RadioStateMachine<'a, RxDisable> {
    fn from(r: RadioStateMachine<'a, RxIdle>) -> Self {
        RadioStateMachine {
            radio: r.radio,
            state: RxDisable,
        }
    }
}
impl<'a> From<RadioStateMachine<'a, Rx>> for RadioStateMachine<'a, RxDisable> {
    fn from(r: RadioStateMachine<'a, Rx>) -> Self {
        RadioStateMachine {
            radio: r.radio,
            state: RxDisable,
        }
    }
}
// Disabled, TxIdle, RxIdle --> TxRu
impl<'a> From<RadioStateMachine<'a, Disabled>> for RadioStateMachine<'a, TxRu> {
    fn from(r: RadioStateMachine<'a, Disabled>) -> Self {
        RadioStateMachine {
            radio: r.radio,
            state: TxRu,
        }
    }
}
impl<'a> From<RadioStateMachine<'a, TxIdle>> for RadioStateMachine<'a, TxRu> {
    fn from(r: RadioStateMachine<'a, TxIdle>) -> Self {
        RadioStateMachine {
            radio: r.radio,
            state: TxRu,
        }
    }
}
impl<'a> From<RadioStateMachine<'a, RxIdle>> for RadioStateMachine<'a, TxRu> {
    fn from(r: RadioStateMachine<'a, RxIdle>) -> Self {
        RadioStateMachine {
            radio: r.radio,
            state: TxRu,
        }
    }
}

// Disabled, TxIdle, RxIdle --> RxRu
impl<'a> From<RadioStateMachine<'a, Disabled>> for RadioStateMachine<'a, RxRu> {
    fn from(r: RadioStateMachine<'a, Disabled>) -> Self {
        RadioStateMachine {
            radio: r.radio,
            state: RxRu,
        }
    }
}
impl<'a> From<RadioStateMachine<'a, TxIdle>> for RadioStateMachine<'a, RxRu> {
    fn from(r: RadioStateMachine<'a, TxIdle>) -> Self {
        RadioStateMachine {
            radio: r.radio,
            state: RxRu,
        }
    }
}
impl<'a> From<RadioStateMachine<'a, RxIdle>> for RadioStateMachine<'a, RxRu> {
    fn from(r: RadioStateMachine<'a, RxIdle>) -> Self {
        RadioStateMachine {
            radio: r.radio,
            state: RxRu,
        }
    }
}
// TxRu, Tx--> TxIdle
impl<'a> From<RadioStateMachine<'a, TxRu>> for RadioStateMachine<'a, TxIdle> {
    fn from(r: RadioStateMachine<'a, TxRu>) -> Self {
        RadioStateMachine {
            radio: r.radio,
            state: TxIdle,
        }
    }
}
impl<'a> From<RadioStateMachine<'a, Tx>> for RadioStateMachine<'a, TxIdle> {
    fn from(r: RadioStateMachine<'a, Tx>) -> Self {
        RadioStateMachine {
            radio: r.radio,
            state: TxIdle,
        }
    }
}
// RxRu, Rx --> RxIdle
impl<'a> From<RadioStateMachine<'a, RxRu>> for RadioStateMachine<'a, RxIdle> {
    fn from(r: RadioStateMachine<'a, RxRu>) -> Self {
        RadioStateMachine {
            radio: r.radio,
            state: RxIdle,
        }
    }
}
impl<'a> From<RadioStateMachine<'a, Rx>> for RadioStateMachine<'a, RxIdle> {
    fn from(r: RadioStateMachine<'a, Rx>) -> Self {
        RadioStateMachine {
            radio: r.radio,
            state: RxIdle,
        }
    }
}
// TxIdle --> Tx
impl<'a> From<RadioStateMachine<'a, TxIdle>> for RadioStateMachine<'a, Tx> {
    fn from(r: RadioStateMachine<'a, TxIdle>) -> Self {
        RadioStateMachine {
            radio: r.radio,
            state: Tx,
        }
    }
}
// RxIdle --> Rx
impl<'a> From<RadioStateMachine<'a, RxIdle>> for RadioStateMachine<'a, Rx> {
    fn from(r: RadioStateMachine<'a, RxIdle>) -> Self {
        RadioStateMachine {
            radio: r.radio,
            state: Rx,
        }
    }
}

enum RadioStateMachineWrapper<'a> {
    Disabled(RadioStateMachine<'a, Disabled>),
    RxRu(RadioStateMachine<'a, RxRu>),
    RxIdle(RadioStateMachine<'a, RxIdle>),
    Rx(RadioStateMachine<'a, Rx>),
    TxRu(RadioStateMachine<'a, TxRu>),
    TxIdle(RadioStateMachine<'a, TxIdle>),
    Tx(RadioStateMachine<'a, Tx>),
    RxDisable(RadioStateMachine<'a, RxDisable>),
    TxDisable(RadioStateMachine<'a, TxDisable>),
}

pub struct Ieee802154Radio<'a> {
    radio_state_machine: RadioStateMachineWrapper<'a>,
}

impl<'a> Ieee802154Radio<'a> {
    pub const fn new() -> Self {
        // Start from the DISABLED state
        Ieee802154Radio {
            radio_state_machine: RadioStateMachineWrapper::Disabled(RadioStateMachine::new()),
        }
    }

    pub fn set_timer_ref(&self, timer: &'a crate::timer::TimerAlarm<'a>) {
        // The timer ref should only be set when the radio is initialized,
        // which is when it's in Disabled state
        match &self.radio_state_machine {
            RadioStateMachineWrapper::Disabled(val) => val.radio.set_timer_ref(timer),
            _ => kernel::debug!("nrf 802.15.4 radio's timer ref can only be set once!"),
        }
    }

    pub fn is_enabled(&self) -> bool {
        // We can check if the radio is enabled or not from any state.
        match &self.radio_state_machine {
            RadioStateMachineWrapper::Disabled(val) => val.radio.is_enabled(),
            RadioStateMachineWrapper::RxRu(val) => val.radio.is_enabled(),
            RadioStateMachineWrapper::RxIdle(val) => val.radio.is_enabled(),
            RadioStateMachineWrapper::Rx(val) => val.radio.is_enabled(),
            RadioStateMachineWrapper::TxRu(val) => val.radio.is_enabled(),
            RadioStateMachineWrapper::TxIdle(val) => val.radio.is_enabled(),
            RadioStateMachineWrapper::Tx(val) => val.radio.is_enabled(),
            RadioStateMachineWrapper::RxDisable(val) => val.radio.is_enabled(),
            RadioStateMachineWrapper::TxDisable(val) => val.radio.is_enabled(),
        }
    }

    #[inline(never)]
    pub fn handle_interrupt(mut self) {
        // State transitions happen during interrupts, which are triggered by events
        match self.radio_state_machine {
            RadioStateMachineWrapper::Disabled(val) => {
                val.radio.handle_interrupt();
                self.radio_state_machine = val.state_transition();
            }
            RadioStateMachineWrapper::RxRu(val) => {
                val.radio.handle_interrupt();
                self.radio_state_machine = val.state_transition();
            }
            RadioStateMachineWrapper::RxIdle(val) => {
                val.radio.handle_interrupt();
                self.radio_state_machine = val.state_transition();
            }
            RadioStateMachineWrapper::Rx(val) => {
                val.radio.handle_interrupt();
                self.radio_state_machine = val.state_transition();
            }
            RadioStateMachineWrapper::TxRu(val) => {
                val.radio.handle_interrupt();
                self.radio_state_machine = val.state_transition();
            }
            RadioStateMachineWrapper::TxIdle(val) => {
                val.radio.handle_interrupt();
                self.radio_state_machine = val.state_transition();
            }
            RadioStateMachineWrapper::Tx(val) => {
                val.radio.handle_interrupt();
                self.radio_state_machine = val.state_transition();
            }
            RadioStateMachineWrapper::RxDisable(val) => {
                val.radio.handle_interrupt();
                self.radio_state_machine = val.state_transition();
            }
            RadioStateMachineWrapper::TxDisable(val) => {
                val.radio.handle_interrupt();
                self.radio_state_machine = val.state_transition();
            }
        }
    }
}

impl<'a> AlarmClient for Ieee802154Radio<'a> {
    fn alarm(&self) {
        // We can access the alarm from any state
        match &self.radio_state_machine {
            RadioStateMachineWrapper::Disabled(val) => val.radio.alarm(),
            RadioStateMachineWrapper::RxRu(val) => val.radio.alarm(),
            RadioStateMachineWrapper::RxIdle(val) => val.radio.alarm(),
            RadioStateMachineWrapper::Rx(val) => val.radio.alarm(),
            RadioStateMachineWrapper::TxRu(val) => val.radio.alarm(),
            RadioStateMachineWrapper::TxIdle(val) => val.radio.alarm(),
            RadioStateMachineWrapper::Tx(val) => val.radio.alarm(),
            RadioStateMachineWrapper::RxDisable(val) => val.radio.alarm(),
            RadioStateMachineWrapper::TxDisable(val) => val.radio.alarm(),
        }
    }
}

impl<'a> kernel::hil::radio::RadioConfig for Ieee802154Radio<'a> {
    fn initialize(
        &self,
        _spi_buf: &'static mut [u8],
        _reg_write: &'static mut [u8],
        _reg_read: &'static mut [u8],
    ) -> Result<(), ErrorCode> {
        match &self.radio_state_machine {
            RadioStateMachineWrapper::Disabled(val) => {
                val.radio.initialize(_spi_buf, _reg_write, _reg_read)
            }
            RadioStateMachineWrapper::RxRu(val) => {
                val.radio.initialize(_spi_buf, _reg_write, _reg_read)
            }
            RadioStateMachineWrapper::RxIdle(val) => {
                val.radio.initialize(_spi_buf, _reg_write, _reg_read)
            }
            RadioStateMachineWrapper::Rx(val) => {
                val.radio.initialize(_spi_buf, _reg_write, _reg_read)
            }
            RadioStateMachineWrapper::TxRu(val) => {
                val.radio.initialize(_spi_buf, _reg_write, _reg_read)
            }
            RadioStateMachineWrapper::TxIdle(val) => {
                val.radio.initialize(_spi_buf, _reg_write, _reg_read)
            }
            RadioStateMachineWrapper::Tx(val) => {
                val.radio.initialize(_spi_buf, _reg_write, _reg_read)
            }
            RadioStateMachineWrapper::RxDisable(val) => {
                val.radio.initialize(_spi_buf, _reg_write, _reg_read)
            }
            RadioStateMachineWrapper::TxDisable(val) => {
                val.radio.initialize(_spi_buf, _reg_write, _reg_read)
            }
        }
    }

    fn reset(&self) -> Result<(), ErrorCode> {
        match &self.radio_state_machine {
            RadioStateMachineWrapper::Disabled(val) => val.radio.reset(),
            RadioStateMachineWrapper::RxRu(val) => val.radio.reset(),
            RadioStateMachineWrapper::RxIdle(val) => val.radio.reset(),
            RadioStateMachineWrapper::Rx(val) => val.radio.reset(),
            RadioStateMachineWrapper::TxRu(val) => val.radio.reset(),
            RadioStateMachineWrapper::TxIdle(val) => val.radio.reset(),
            RadioStateMachineWrapper::Tx(val) => val.radio.reset(),
            RadioStateMachineWrapper::RxDisable(val) => val.radio.reset(),
            RadioStateMachineWrapper::TxDisable(val) => val.radio.reset(),
        }
    }

    fn start(&self) -> Result<(), ErrorCode> {
        match &self.radio_state_machine {
            RadioStateMachineWrapper::Disabled(val) => val.radio.start(),
            RadioStateMachineWrapper::RxRu(val) => val.radio.start(),
            RadioStateMachineWrapper::RxIdle(val) => val.radio.start(),
            RadioStateMachineWrapper::Rx(val) => val.radio.start(),
            RadioStateMachineWrapper::TxRu(val) => val.radio.start(),
            RadioStateMachineWrapper::TxIdle(val) => val.radio.start(),
            RadioStateMachineWrapper::Tx(val) => val.radio.start(),
            RadioStateMachineWrapper::RxDisable(val) => val.radio.start(),
            RadioStateMachineWrapper::TxDisable(val) => val.radio.start(),
        }
    }

    fn stop(&self) -> Result<(), ErrorCode> {
        match &self.radio_state_machine {
            RadioStateMachineWrapper::Disabled(val) => val.radio.stop(),
            RadioStateMachineWrapper::RxRu(val) => val.radio.stop(),
            RadioStateMachineWrapper::RxIdle(val) => val.radio.stop(),
            RadioStateMachineWrapper::Rx(val) => val.radio.stop(),
            RadioStateMachineWrapper::TxRu(val) => val.radio.stop(),
            RadioStateMachineWrapper::TxIdle(val) => val.radio.stop(),
            RadioStateMachineWrapper::Tx(val) => val.radio.stop(),
            RadioStateMachineWrapper::RxDisable(val) => val.radio.stop(),
            RadioStateMachineWrapper::TxDisable(val) => val.radio.stop(),
        }
    }

    fn is_on(&self) -> bool {
        match &self.radio_state_machine {
            RadioStateMachineWrapper::Disabled(val) => val.radio.is_on(),
            RadioStateMachineWrapper::RxRu(val) => val.radio.is_on(),
            RadioStateMachineWrapper::RxIdle(val) => val.radio.is_on(),
            RadioStateMachineWrapper::Rx(val) => val.radio.is_on(),
            RadioStateMachineWrapper::TxRu(val) => val.radio.is_on(),
            RadioStateMachineWrapper::TxIdle(val) => val.radio.is_on(),
            RadioStateMachineWrapper::Tx(val) => val.radio.is_on(),
            RadioStateMachineWrapper::RxDisable(val) => val.radio.is_on(),
            RadioStateMachineWrapper::TxDisable(val) => val.radio.is_on(),
        }
    }

    fn busy(&self) -> bool {
        match &self.radio_state_machine {
            RadioStateMachineWrapper::Disabled(val) => val.radio.busy(),
            RadioStateMachineWrapper::RxRu(val) => val.radio.busy(),
            RadioStateMachineWrapper::RxIdle(val) => val.radio.busy(),
            RadioStateMachineWrapper::Rx(val) => val.radio.busy(),
            RadioStateMachineWrapper::TxRu(val) => val.radio.busy(),
            RadioStateMachineWrapper::TxIdle(val) => val.radio.busy(),
            RadioStateMachineWrapper::Tx(val) => val.radio.busy(),
            RadioStateMachineWrapper::RxDisable(val) => val.radio.busy(),
            RadioStateMachineWrapper::TxDisable(val) => val.radio.busy(),
        }
    }

    fn set_power_client(&self, client: &'static dyn PowerClient) {
        match &self.radio_state_machine {
            RadioStateMachineWrapper::Disabled(val) => val.radio.set_power_client(client),
            RadioStateMachineWrapper::RxRu(val) => val.radio.set_power_client(client),
            RadioStateMachineWrapper::RxIdle(val) => val.radio.set_power_client(client),
            RadioStateMachineWrapper::Rx(val) => val.radio.set_power_client(client),
            RadioStateMachineWrapper::TxRu(val) => val.radio.set_power_client(client),
            RadioStateMachineWrapper::TxIdle(val) => val.radio.set_power_client(client),
            RadioStateMachineWrapper::Tx(val) => val.radio.set_power_client(client),
            RadioStateMachineWrapper::RxDisable(val) => val.radio.set_power_client(client),
            RadioStateMachineWrapper::TxDisable(val) => val.radio.set_power_client(client),
        }
    }

    fn config_commit(&self) {
        match &self.radio_state_machine {
            RadioStateMachineWrapper::Disabled(val) => val.radio.config_commit(),
            RadioStateMachineWrapper::RxRu(val) => val.radio.config_commit(),
            RadioStateMachineWrapper::RxIdle(val) => val.radio.config_commit(),
            RadioStateMachineWrapper::Rx(val) => val.radio.config_commit(),
            RadioStateMachineWrapper::TxRu(val) => val.radio.config_commit(),
            RadioStateMachineWrapper::TxIdle(val) => val.radio.config_commit(),
            RadioStateMachineWrapper::Tx(val) => val.radio.config_commit(),
            RadioStateMachineWrapper::RxDisable(val) => val.radio.config_commit(),
            RadioStateMachineWrapper::TxDisable(val) => val.radio.config_commit(),
        }
    }

    fn set_config_client(&self, client: &'static dyn radio::ConfigClient) {
        match &self.radio_state_machine {
            RadioStateMachineWrapper::Disabled(val) => val.radio.set_config_client(client),
            RadioStateMachineWrapper::RxRu(val) => val.radio.set_config_client(client),
            RadioStateMachineWrapper::RxIdle(val) => val.radio.set_config_client(client),
            RadioStateMachineWrapper::Rx(val) => val.radio.set_config_client(client),
            RadioStateMachineWrapper::TxRu(val) => val.radio.set_config_client(client),
            RadioStateMachineWrapper::TxIdle(val) => val.radio.set_config_client(client),
            RadioStateMachineWrapper::Tx(val) => val.radio.set_config_client(client),
            RadioStateMachineWrapper::RxDisable(val) => val.radio.set_config_client(client),
            RadioStateMachineWrapper::TxDisable(val) => val.radio.set_config_client(client),
        }
    }

    fn get_address(&self) -> u16 {
        match &self.radio_state_machine {
            RadioStateMachineWrapper::Disabled(val) => val.radio.get_address(),
            RadioStateMachineWrapper::RxRu(val) => val.radio.get_address(),
            RadioStateMachineWrapper::RxIdle(val) => val.radio.get_address(),
            RadioStateMachineWrapper::Rx(val) => val.radio.get_address(),
            RadioStateMachineWrapper::TxRu(val) => val.radio.get_address(),
            RadioStateMachineWrapper::TxIdle(val) => val.radio.get_address(),
            RadioStateMachineWrapper::Tx(val) => val.radio.get_address(),
            RadioStateMachineWrapper::RxDisable(val) => val.radio.get_address(),
            RadioStateMachineWrapper::TxDisable(val) => val.radio.get_address(),
        }
    }

    fn get_address_long(&self) -> [u8; 8] {
        match &self.radio_state_machine {
            RadioStateMachineWrapper::Disabled(val) => val.radio.get_address_long(),
            RadioStateMachineWrapper::RxRu(val) => val.radio.get_address_long(),
            RadioStateMachineWrapper::RxIdle(val) => val.radio.get_address_long(),
            RadioStateMachineWrapper::Rx(val) => val.radio.get_address_long(),
            RadioStateMachineWrapper::TxRu(val) => val.radio.get_address_long(),
            RadioStateMachineWrapper::TxIdle(val) => val.radio.get_address_long(),
            RadioStateMachineWrapper::Tx(val) => val.radio.get_address_long(),
            RadioStateMachineWrapper::RxDisable(val) => val.radio.get_address_long(),
            RadioStateMachineWrapper::TxDisable(val) => val.radio.get_address_long(),
        }
    }

    fn get_pan(&self) -> u16 {
        match &self.radio_state_machine {
            RadioStateMachineWrapper::Disabled(val) => val.radio.get_pan(),
            RadioStateMachineWrapper::RxRu(val) => val.radio.get_pan(),
            RadioStateMachineWrapper::RxIdle(val) => val.radio.get_pan(),
            RadioStateMachineWrapper::Rx(val) => val.radio.get_pan(),
            RadioStateMachineWrapper::TxRu(val) => val.radio.get_pan(),
            RadioStateMachineWrapper::TxIdle(val) => val.radio.get_pan(),
            RadioStateMachineWrapper::Tx(val) => val.radio.get_pan(),
            RadioStateMachineWrapper::RxDisable(val) => val.radio.get_pan(),
            RadioStateMachineWrapper::TxDisable(val) => val.radio.get_pan(),
        }
    }

    fn get_tx_power(&self) -> i8 {
        match &self.radio_state_machine {
            RadioStateMachineWrapper::Disabled(val) => val.radio.get_tx_power(),
            RadioStateMachineWrapper::RxRu(val) => val.radio.get_tx_power(),
            RadioStateMachineWrapper::RxIdle(val) => val.radio.get_tx_power(),
            RadioStateMachineWrapper::Rx(val) => val.radio.get_tx_power(),
            RadioStateMachineWrapper::TxRu(val) => val.radio.get_tx_power(),
            RadioStateMachineWrapper::TxIdle(val) => val.radio.get_tx_power(),
            RadioStateMachineWrapper::Tx(val) => val.radio.get_tx_power(),
            RadioStateMachineWrapper::RxDisable(val) => val.radio.get_tx_power(),
            RadioStateMachineWrapper::TxDisable(val) => val.radio.get_tx_power(),
        }
    }

    fn get_channel(&self) -> u8 {
        match &self.radio_state_machine {
            RadioStateMachineWrapper::Disabled(val) => val.radio.get_channel(),
            RadioStateMachineWrapper::RxRu(val) => val.radio.get_channel(),
            RadioStateMachineWrapper::RxIdle(val) => val.radio.get_channel(),
            RadioStateMachineWrapper::Rx(val) => val.radio.get_channel(),
            RadioStateMachineWrapper::TxRu(val) => val.radio.get_channel(),
            RadioStateMachineWrapper::TxIdle(val) => val.radio.get_channel(),
            RadioStateMachineWrapper::Tx(val) => val.radio.get_channel(),
            RadioStateMachineWrapper::RxDisable(val) => val.radio.get_channel(),
            RadioStateMachineWrapper::TxDisable(val) => val.radio.get_channel(),
        }
    }

    fn set_address(&self, addr: u16) {
        match &self.radio_state_machine {
            RadioStateMachineWrapper::Disabled(val) => val.radio.set_address(addr),
            RadioStateMachineWrapper::RxRu(val) => val.radio.set_address(addr),
            RadioStateMachineWrapper::RxIdle(val) => val.radio.set_address(addr),
            RadioStateMachineWrapper::Rx(val) => val.radio.set_address(addr),
            RadioStateMachineWrapper::TxRu(val) => val.radio.set_address(addr),
            RadioStateMachineWrapper::TxIdle(val) => val.radio.set_address(addr),
            RadioStateMachineWrapper::Tx(val) => val.radio.set_address(addr),
            RadioStateMachineWrapper::RxDisable(val) => val.radio.set_address(addr),
            RadioStateMachineWrapper::TxDisable(val) => val.radio.set_address(addr),
        }
    }

    fn set_address_long(&self, addr: [u8; 8]) {
        match &self.radio_state_machine {
            RadioStateMachineWrapper::Disabled(val) => val.radio.set_address_long(addr),
            RadioStateMachineWrapper::RxRu(val) => val.radio.set_address_long(addr),
            RadioStateMachineWrapper::RxIdle(val) => val.radio.set_address_long(addr),
            RadioStateMachineWrapper::Rx(val) => val.radio.set_address_long(addr),
            RadioStateMachineWrapper::TxRu(val) => val.radio.set_address_long(addr),
            RadioStateMachineWrapper::TxIdle(val) => val.radio.set_address_long(addr),
            RadioStateMachineWrapper::Tx(val) => val.radio.set_address_long(addr),
            RadioStateMachineWrapper::RxDisable(val) => val.radio.set_address_long(addr),
            RadioStateMachineWrapper::TxDisable(val) => val.radio.set_address_long(addr),
        }
    }

    fn set_pan(&self, id: u16) {
        match &self.radio_state_machine {
            RadioStateMachineWrapper::Disabled(val) => val.radio.set_pan(id),
            RadioStateMachineWrapper::RxRu(val) => val.radio.set_pan(id),
            RadioStateMachineWrapper::RxIdle(val) => val.radio.set_pan(id),
            RadioStateMachineWrapper::Rx(val) => val.radio.set_pan(id),
            RadioStateMachineWrapper::TxRu(val) => val.radio.set_pan(id),
            RadioStateMachineWrapper::TxIdle(val) => val.radio.set_pan(id),
            RadioStateMachineWrapper::Tx(val) => val.radio.set_pan(id),
            RadioStateMachineWrapper::RxDisable(val) => val.radio.set_pan(id),
            RadioStateMachineWrapper::TxDisable(val) => val.radio.set_pan(id),
        }
    }

    fn set_tx_power(&self, power: i8) -> Result<(), ErrorCode> {
        match &self.radio_state_machine {
            RadioStateMachineWrapper::Disabled(val) => {
                kernel::hil::radio::RadioConfig::set_tx_power(&val.radio, power)
            }
            RadioStateMachineWrapper::RxRu(val) => {
                kernel::hil::radio::RadioConfig::set_tx_power(&val.radio, power)
            }
            RadioStateMachineWrapper::RxIdle(val) => {
                kernel::hil::radio::RadioConfig::set_tx_power(&val.radio, power)
            }
            RadioStateMachineWrapper::Rx(val) => {
                kernel::hil::radio::RadioConfig::set_tx_power(&val.radio, power)
            }
            RadioStateMachineWrapper::TxRu(val) => {
                kernel::hil::radio::RadioConfig::set_tx_power(&val.radio, power)
            }
            RadioStateMachineWrapper::TxIdle(val) => {
                kernel::hil::radio::RadioConfig::set_tx_power(&val.radio, power)
            }
            RadioStateMachineWrapper::Tx(val) => {
                kernel::hil::radio::RadioConfig::set_tx_power(&val.radio, power)
            }
            RadioStateMachineWrapper::RxDisable(val) => {
                kernel::hil::radio::RadioConfig::set_tx_power(&val.radio, power)
            }
            RadioStateMachineWrapper::TxDisable(val) => {
                kernel::hil::radio::RadioConfig::set_tx_power(&val.radio, power)
            }
        }
    }

    fn set_channel(&self, chan: u8) -> Result<(), ErrorCode> {
        match &self.radio_state_machine {
            RadioStateMachineWrapper::Disabled(val) => val.radio.set_channel(chan),
            RadioStateMachineWrapper::RxRu(val) => val.radio.set_channel(chan),
            RadioStateMachineWrapper::RxIdle(val) => val.radio.set_channel(chan),
            RadioStateMachineWrapper::Rx(val) => val.radio.set_channel(chan),
            RadioStateMachineWrapper::TxRu(val) => val.radio.set_channel(chan),
            RadioStateMachineWrapper::TxIdle(val) => val.radio.set_channel(chan),
            RadioStateMachineWrapper::Tx(val) => val.radio.set_channel(chan),
            RadioStateMachineWrapper::RxDisable(val) => val.radio.set_channel(chan),
            RadioStateMachineWrapper::TxDisable(val) => val.radio.set_channel(chan),
        }
    }
}

impl<'a> kernel::hil::radio::RadioData for Ieee802154Radio<'a> {
    fn set_transmit_client(&self, client: &'static dyn radio::TxClient) {
        match &self.radio_state_machine {
            RadioStateMachineWrapper::Disabled(val) => val.radio.set_transmit_client(client),
            RadioStateMachineWrapper::RxRu(val) => val.radio.set_transmit_client(client),
            RadioStateMachineWrapper::RxIdle(val) => val.radio.set_transmit_client(client),
            RadioStateMachineWrapper::Rx(val) => val.radio.set_transmit_client(client),
            RadioStateMachineWrapper::TxRu(val) => val.radio.set_transmit_client(client),
            RadioStateMachineWrapper::TxIdle(val) => val.radio.set_transmit_client(client),
            RadioStateMachineWrapper::Tx(val) => val.radio.set_transmit_client(client),
            RadioStateMachineWrapper::RxDisable(val) => val.radio.set_transmit_client(client),
            RadioStateMachineWrapper::TxDisable(val) => val.radio.set_transmit_client(client),
        }
    }

    fn set_receive_client(
        &self,
        client: &'static dyn radio::RxClient,
        receive_buffer: &'static mut [u8],
    ) {
        match &self.radio_state_machine {
            RadioStateMachineWrapper::Disabled(val) => {
                val.radio.set_receive_client(client, receive_buffer)
            }
            RadioStateMachineWrapper::RxRu(val) => {
                val.radio.set_receive_client(client, receive_buffer)
            }
            RadioStateMachineWrapper::RxIdle(val) => {
                val.radio.set_receive_client(client, receive_buffer)
            }
            RadioStateMachineWrapper::Rx(val) => {
                val.radio.set_receive_client(client, receive_buffer)
            }
            RadioStateMachineWrapper::TxRu(val) => {
                val.radio.set_receive_client(client, receive_buffer)
            }
            RadioStateMachineWrapper::TxIdle(val) => {
                val.radio.set_receive_client(client, receive_buffer)
            }
            RadioStateMachineWrapper::Tx(val) => {
                val.radio.set_receive_client(client, receive_buffer)
            }
            RadioStateMachineWrapper::RxDisable(val) => {
                val.radio.set_receive_client(client, receive_buffer)
            }
            RadioStateMachineWrapper::TxDisable(val) => {
                val.radio.set_receive_client(client, receive_buffer)
            }
        }
    }

    fn set_receive_buffer(&self, receive_buffer: &'static mut [u8]) {
        match &self.radio_state_machine {
            RadioStateMachineWrapper::Disabled(val) => val.radio.set_receive_buffer(receive_buffer),
            RadioStateMachineWrapper::RxRu(val) => val.radio.set_receive_buffer(receive_buffer),
            RadioStateMachineWrapper::RxIdle(val) => val.radio.set_receive_buffer(receive_buffer),
            RadioStateMachineWrapper::Rx(val) => val.radio.set_receive_buffer(receive_buffer),
            RadioStateMachineWrapper::TxRu(val) => val.radio.set_receive_buffer(receive_buffer),
            RadioStateMachineWrapper::TxIdle(val) => val.radio.set_receive_buffer(receive_buffer),
            RadioStateMachineWrapper::Tx(val) => val.radio.set_receive_buffer(receive_buffer),
            RadioStateMachineWrapper::RxDisable(val) => {
                val.radio.set_receive_buffer(receive_buffer)
            }
            RadioStateMachineWrapper::TxDisable(val) => {
                val.radio.set_receive_buffer(receive_buffer)
            }
        }
    }

    fn transmit(
        &self,
        spi_buf: &'static mut [u8],
        frame_len: usize,
    ) -> Result<(), (ErrorCode, &'static mut [u8])> {
        match &self.radio_state_machine {
            RadioStateMachineWrapper::Disabled(val) => val.radio.transmit(spi_buf, frame_len),
            RadioStateMachineWrapper::RxRu(val) => val.radio.transmit(spi_buf, frame_len),
            RadioStateMachineWrapper::RxIdle(val) => val.radio.transmit(spi_buf, frame_len),
            RadioStateMachineWrapper::Rx(val) => val.radio.transmit(spi_buf, frame_len),
            RadioStateMachineWrapper::TxRu(val) => val.radio.transmit(spi_buf, frame_len),
            RadioStateMachineWrapper::TxIdle(val) => val.radio.transmit(spi_buf, frame_len),
            RadioStateMachineWrapper::Tx(val) => val.radio.transmit(spi_buf, frame_len),
            RadioStateMachineWrapper::RxDisable(val) => val.radio.transmit(spi_buf, frame_len),
            RadioStateMachineWrapper::TxDisable(val) => val.radio.transmit(spi_buf, frame_len),
        }
    }
}
