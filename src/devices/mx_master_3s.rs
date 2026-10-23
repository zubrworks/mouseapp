//! MX Master 3S model description.

use super::{Button, DpiRange, Model, ThresholdRange};
use crate::hidpp::features::{battery, controls, dpi, host, name, smartshift, thumbwheel, wheel};

/// Control IDs follow Logitech's ReprogControlsV4 table.
const CID_LEFT: u16 = 0x0050;
const CID_RIGHT: u16 = 0x0051;
const CID_MIDDLE: u16 = 0x0052;
const CID_BACK: u16 = 0x0053;
const CID_FORWARD: u16 = 0x0056;
const CID_GESTURE: u16 = 0x00C3;
const CID_SMARTSHIFT: u16 = 0x00C4;

/// Bluetooth product ID; over a Bolt receiver the model is matched by name.
const PID_BLUETOOTH: u16 = 0xB034;

pub static MODEL: Model = Model {
    name: "MX Master 3S",
    pids: &[PID_BLUETOOTH],
    buttons: &[
        Button { cid: CID_LEFT, key: "left", remappable: false },
        Button { cid: CID_RIGHT, key: "right", remappable: false },
        Button { cid: CID_MIDDLE, key: "middle", remappable: true },
        Button { cid: CID_BACK, key: "back", remappable: true },
        Button { cid: CID_FORWARD, key: "forward", remappable: true },
        Button { cid: CID_GESTURE, key: "gesture", remappable: false },
        Button { cid: CID_SMARTSHIFT, key: "smartshift", remappable: true },
    ],
    dpi: DpiRange { min: 200, max: 8000, step: 50 },
    default_dpi: 1000,
    // Threshold 255 means "always ratcheted"; default is a fallback when the mouse reports none.
    smartshift: ThresholdRange { min: 1, max: smartshift::ALWAYS_RATCHET, default: 10 },
    features: &[
        name::ID,
        battery::ID,
        host::ID,
        controls::ID,
        // This unit reports the legacy SmartShift; the enhanced one is handled in smartshift.rs.
        smartshift::LEGACY_ID,
        wheel::ID,
        thumbwheel::ID,
        dpi::ID,
    ],
};
