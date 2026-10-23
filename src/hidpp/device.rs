//! A HID++ 2.0 device: ping, protocol version, feature table.

use std::collections::HashMap;
use std::ffi::CString;
use std::ops::RangeInclusive;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use hidapi::HidApi;
use rust_i18n::t;

use super::features::{
    self, battery::{self, Charge}, controls, dpi, fwinfo, host, name, smartshift, thumbwheel, wheel,
};
use super::message::{HidppError, Message};
use super::transport::{Interrupter, IoFailed, Transport};
use crate::devices;
use crate::discovery::{self, Connection, Found};
use crate::util::lock;

/// Software IDs of our requests (non-zero; notifications carry 0). Each
/// request takes the next one, so two requests waiting at the same time
/// (the mouse thread and the check thread) never expect the same response,
/// and a late response to a request that timed out is not taken for the
/// answer to the next one.
///
/// macOS hands every input report to all processes that have the device
/// open, so the daemon and a CLI command or the app must not share IDs:
/// they would take each other's responses. The daemon uses the upper half
/// of the pool, everything else the lower half.
const DAEMON_SW_IDS: RangeInclusive<u8> = 0x08..=0x0F;
const CLIENT_SW_IDS: RangeInclusive<u8> = 0x01..=0x07;

static IS_DAEMON: AtomicBool = AtomicBool::new(false);

/// Marks this process as the daemon; call before opening any device.
pub fn use_daemon_sw_ids() {
    IS_DAEMON.store(true, Ordering::Relaxed);
}

fn process_sw_ids() -> RangeInclusive<u8> {
    if IS_DAEMON.load(Ordering::Relaxed) { DAEMON_SW_IDS } else { CLIENT_SW_IDS }
}
/// Feature index of the root feature (always 0).
const ROOT_INDEX: u8 = 0x00;
/// Feature ID of the root feature.
pub const ROOT_ID: u16 = 0x0000;
/// Feature ID of FeatureSet.
pub const FEATURE_SET_ID: u16 = 0x0001;
/// Root feature functions.
const ROOT_GET_FEATURE: u8 = 0;
const ROOT_PING: u8 = 1;
/// FeatureSet functions.
const SET_GET_COUNT: u8 = 0;
const SET_GET_FEATURE_ID: u8 = 1;
/// Payload byte echoed back by a ping.
const PING_DATA: u8 = 0x5B;
/// Slots a Bolt/Unifying receiver can hold.
const RECEIVER_SLOTS: std::ops::RangeInclusive<u8> = 1..=6;
/// Device index for a directly connected (Bluetooth/USB) device.
const INDEX_DIRECT: u8 = 0xFF;
/// How long to wait for a response.
const TIMEOUT: Duration = Duration::from_millis(1500);
/// How often a request is repeated when the device answers "busy".
const BUSY_RETRIES: u32 = 3;
/// Pause before repeating a request the device found busy.
const BUSY_DELAY: Duration = Duration::from_millis(60);
/// A call slower than this is logged.
const SLOW_CALL: Duration = Duration::from_millis(300);
/// The same for the root feature (the idle ping): it is slow on a sleeping
/// mouse, so only a really long one is worth a line.
const SLOW_ROOT_CALL: Duration = Duration::from_millis(1000);

/// Whether a call that took `took` is logged as slow.
fn is_slow_call(feature_index: u8, took: Duration) -> bool {
    let limit = if feature_index == ROOT_INDEX { SLOW_ROOT_CALL } else { SLOW_CALL };
    took > limit
}

/// The device did not answer a request in time.
#[derive(Debug, thiserror::Error)]
#[error("timeout waiting for device response")]
pub struct Timeout;

/// True if `e` means the link to the mouse is gone or unusable (timeout,
/// HID I/O failure, receiver says the mouse is unreachable), as opposed to
/// the mouse rejecting one request or a setting that does not fit it.
pub fn is_link_failure(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Timeout>().is_some()
        || e.downcast_ref::<IoFailed>().is_some()
        || e.downcast_ref::<HidppError>().is_some_and(HidppError::is_link_down)
}

/// What the device's reports and writes looked like since the last
/// [`Device::take_link_stats`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinkStats {
    /// Reports that are not HID++ (e.g. ordinary mouse reports on the same
    /// handle), dropped by the reader thread.
    pub non_hidpp: u32,
    /// Report ID of the latest of them.
    pub non_hidpp_id: Option<u8>,
    /// HID++ reports addressed to another device index (dropped).
    pub other_index: u32,
    /// HID++ reports lost because the reader's queue was full.
    pub queue_dropped: u32,
    /// Longest single write to the mouse.
    pub max_write: Duration,
}

/// One entry of the device's feature table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeatureInfo {
    pub index: u8,
    pub id: u16,
    pub version: u8,
}

/// Hands out software IDs for requests, see [`DAEMON_SW_IDS`].
#[derive(Debug)]
struct SwIds {
    range: RangeInclusive<u8>,
    next: u8,
    /// (feature index, function, software ID) of the requests waiting now.
    busy: Vec<(u8, u8, u8)>,
}

impl Default for SwIds {
    fn default() -> Self {
        SwIds::new(process_sw_ids())
    }
}

impl SwIds {
    fn new(range: RangeInclusive<u8>) -> Self {
        SwIds { next: *range.start(), range, busy: Vec::new() }
    }

    /// The next ID in turn that no waiting request to the same feature and
    /// function uses; marked busy until [`Self::release`].
    fn take(&mut self, feature_index: u8, function: u8) -> u8 {
        let mut id = self.next;
        for _ in self.range.clone() {
            if !self.busy.contains(&(feature_index, function, id)) {
                break;
            }
            id = self.after(id);
        }
        self.next = self.after(id);
        self.busy.push((feature_index, function, id));
        id
    }

    fn release(&mut self, feature_index: u8, function: u8, id: u8) {
        if let Some(pos) = self.busy.iter().position(|k| *k == (feature_index, function, id)) {
            self.busy.swap_remove(pos);
        }
    }

    fn after(&self, id: u8) -> u8 {
        if id >= *self.range.end() { *self.range.start() } else { id + 1 }
    }
}

/// An open HID++ 2.0 device. Shared by the mouse thread and the check
/// thread: requests from both can wait at the same time, notifications are
/// read without waiting for them.
pub struct Device {
    transport: Transport,
    pub index: u8,
    pub name: String,
    pub pid: u16,
    pub connection: Connection,
    /// HID interface this device was opened on.
    pub path: CString,
    /// Feature index per feature ID (`None` = unsupported); the table does
    /// not change while the device is open.
    features: Mutex<HashMap<u16, Option<u8>>>,
    stats: Mutex<LinkStats>,
    sw_ids: Mutex<SwIds>,
}

/// Result of probing the HID++ interfaces for mice.
#[derive(Default)]
pub struct Scan {
    pub devices: Vec<Device>,
    /// Interfaces that could not be opened (e.g. missing Input Monitoring).
    pub errors: Vec<anyhow::Error>,
}

impl Device {
    fn new(transport: Transport, index: u8, found: &Found) -> Self {
        Device {
            transport,
            index,
            name: found.name.clone(),
            pid: found.pid,
            connection: found.connection,
            path: found.path.clone(),
            features: Mutex::new(HashMap::new()),
            stats: Mutex::new(LinkStats::default()),
            sw_ids: Mutex::new(SwIds::default()),
        }
    }

    /// A directly connected device on a fake transport: `write` gets every
    /// request, the [`Feed`] plays the mouse's answers (tests).
    #[cfg(test)]
    pub(crate) fn fake(
        write: impl FnMut(&[u8]) + Send + 'static,
    ) -> (Self, super::transport::Feed) {
        let (transport, feed) = Transport::fake(write);
        let found = Found {
            name: "Fake mouse".into(),
            pid: 0,
            connection: Connection::Bluetooth,
            usage_page: 0,
            usage: 0,
            path: CString::default(),
        };
        (Device::new(transport, INDEX_DIRECT, &found), feed)
    }

    /// Opens the first responding device on a new HID API handle.
    pub fn open_default() -> Result<Self> {
        Self::open_first(&HidApi::new()?)
    }

    /// Opens the first responding device found on the system.
    fn open_first(api: &HidApi) -> Result<Self> {
        let scan = Self::scan(api, &|_, _| false, 1);
        match scan.devices.into_iter().next() {
            Some(d) => Ok(d),
            None => match scan.errors.into_iter().next_back() {
                Some(e) => Err(e),
                None => bail!("{}", t!("list.none")),
            },
        }
    }

    /// Probes every HID++ interface and returns each device that answers
    /// (several mice can share one receiver). Interface/slot pairs for which
    /// `skip` returns true are not touched. Stops after `limit` devices.
    pub fn scan(api: &HidApi, skip: &dyn Fn(&CString, u8) -> bool, limit: usize) -> Scan {
        let mut out = Scan::default();
        for found in discovery::find_devices(api)
            .iter()
            .filter(|f| discovery::is_long_interface(f))
        {
            let left = limit.saturating_sub(out.devices.len());
            if left == 0 {
                break;
            }
            let part = Self::scan_interface(api, found, skip, left);
            out.devices.extend(part.devices);
            out.errors.extend(part.errors);
        }
        out
    }

    fn scan_interface(
        api: &HidApi,
        found: &Found,
        skip: &dyn Fn(&CString, u8) -> bool,
        limit: usize,
    ) -> Scan {
        let mut out = Scan::default();
        let slots: Vec<u8> = match found.connection {
            Connection::Bolt | Connection::Unifying => RECEIVER_SLOTS.collect(),
            _ => vec![INDEX_DIRECT],
        };
        // One transport is reused for probing; every device that answers
        // keeps its own, so two mice never read each other's reports.
        let mut spare: Option<Transport> = None;
        for index in slots {
            if out.devices.len() >= limit {
                break;
            }
            if skip(&found.path, index) {
                continue;
            }
            let transport = match spare.take() {
                Some(t) => t,
                None => match Transport::open(api, &found.path) {
                    Ok(t) => t,
                    Err(e) => {
                        out.errors.push(e);
                        break;
                    }
                },
            };
            let dev = Device::new(transport, index, found);
            match dev.ping() {
                Ok(_) => out.devices.push(dev),
                Err(e) => {
                    log::debug!("{} slot {index}: no HID++ 2.0 device ({e})", found.name);
                    spare = Some(dev.transport);
                }
            }
        }
        out
    }

    /// Sends a request and waits for the matching response.
    /// Device errors become `Err` (downcast to `HidppError`); no answer in
    /// time is a [`Timeout`]. A "busy" answer is retried a few times.
    /// Notifications that arrive meanwhile stay for [`Self::read_message`].
    pub fn call(&self, feature_index: u8, function: u8, params: &[u8]) -> Result<Vec<u8>> {
        let mut attempt = 0;
        let start = Instant::now();
        let mut write = Duration::ZERO;
        let r = loop {
            match self.call_once(feature_index, function, params, &mut write) {
                Err(e)
                    if attempt < BUSY_RETRIES
                        && e.downcast_ref::<HidppError>().is_some_and(HidppError::is_busy) =>
                {
                    attempt += 1;
                    log::debug!("device busy, retry {attempt}/{BUSY_RETRIES}");
                    sleep(BUSY_DELAY);
                }
                r => break r,
            }
        };
        let took = start.elapsed();
        if is_slow_call(feature_index, took) {
            log::info!(
                "slow call: feature_index={feature_index:#04x} function={function} took_ms={} write_ms={} busy_retries={attempt} ok={}",
                took.as_millis(),
                write.as_millis(),
                r.is_ok()
            );
        }
        r
    }

    /// One request and its response; `write` gets the duration of the write.
    fn call_once(
        &self,
        feature_index: u8,
        function: u8,
        params: &[u8],
        write: &mut Duration,
    ) -> Result<Vec<u8>> {
        let sw_id = lock(&self.sw_ids).take(feature_index, function);
        let r = self.exchange(feature_index, function, sw_id, params, write);
        lock(&self.sw_ids).release(feature_index, function, sw_id);
        r
    }

    fn exchange(
        &self,
        feature_index: u8,
        function: u8,
        sw_id: u8,
        params: &[u8],
        write: &mut Duration,
    ) -> Result<Vec<u8>> {
        let req = Message::request(self.index, feature_index, function, sw_id, params)?;
        let bytes = req.to_bytes();
        let matches = Box::new(move |raw: &[u8]| Message::parse(raw).is_ok_and(|m| m.answers(&req)));
        let ex = self.transport.exchange(&bytes, matches, TIMEOUT)?;
        *write = ex.write;
        self.note_write(ex.write);
        let Some(raw) = ex.reply else {
            return Err(Timeout.into());
        };
        let msg = Message::parse(&raw)?;
        if let Some(e) = msg.as_error() {
            return Err(e.into());
        }
        Ok(msg.params)
    }

    /// Sends a request without waiting for a response (e.g. host switch, after which
    /// the device drops the link).
    pub fn send(&self, feature_index: u8, function: u8, params: &[u8]) -> Result<()> {
        let sw_id = {
            let mut ids = lock(&self.sw_ids);
            let id = ids.take(feature_index, function);
            ids.release(feature_index, function, id);
            id
        };
        let req = Message::request(self.index, feature_index, function, sw_id, params)?;
        let start = Instant::now();
        let r = self.transport.write(&req.to_bytes());
        self.note_write(start.elapsed());
        r
    }

    fn note_write(&self, took: Duration) {
        let mut s = lock(&self.stats);
        s.max_write = s.max_write.max(took);
    }

    fn count_other_index(&self) {
        let mut s = lock(&self.stats);
        s.other_index = s.other_index.saturating_add(1);
    }

    /// Returns the link statistics gathered so far and starts over.
    pub fn take_link_stats(&self) -> LinkStats {
        let reader = self.transport.take_stats();
        LinkStats {
            non_hidpp: reader.non_hidpp,
            non_hidpp_id: reader.non_hidpp_id,
            queue_dropped: reader.dropped,
            ..std::mem::take(&mut *lock(&self.stats))
        }
    }

    /// Waits up to `timeout_ms` for the next message from this device
    /// (notifications such as diverted button events). `None` on timeout or
    /// when woken through [`Self::interrupter`]. Responses to requests that
    /// are waiting never come here.
    pub fn read_message(&self, timeout_ms: i32) -> Result<Option<Message>> {
        let Some(raw) = self.transport.read(timeout_ms)? else {
            return Ok(None);
        };
        let msg = Message::parse(&raw).ok().filter(|m| m.device_index == self.index);
        if msg.is_none() {
            self.count_other_index();
        }
        Ok(msg)
    }

    /// Wakes a thread waiting in [`Self::read_message`] from another thread.
    pub fn interrupter(&self) -> Interrupter {
        self.transport.interrupter()
    }

    /// When the last report arrived from the mouse (a response, a
    /// notification or any other report); `None` if none yet.
    pub fn last_rx(&self) -> Option<Instant> {
        self.transport.last_rx()
    }

    /// Pings the device; returns the protocol version (major, minor).
    pub fn ping(&self) -> Result<(u8, u8)> {
        let r = self.call(ROOT_INDEX, ROOT_PING, &[0, 0, PING_DATA])?;
        match r.as_slice() {
            [major, minor, PING_DATA, ..] => Ok((*major, *minor)),
            _ => bail!("ping payload mismatch"),
        }
    }

    /// Looks up the index of `feature_id` via the root feature (cached);
    /// `None` if unsupported.
    pub fn feature(&self, feature_id: u16) -> Result<Option<u8>> {
        if let Some(found) = lock(&self.features).get(&feature_id) {
            return Ok(*found);
        }
        let r = self.call(ROOT_INDEX, ROOT_GET_FEATURE, &feature_id.to_be_bytes())?;
        let index = r.first().copied().context("empty feature lookup response")?;
        let found = (index != 0).then_some(index);
        lock(&self.features).insert(feature_id, found);
        Ok(found)
    }

    /// Like [`Self::feature`], but an unsupported feature is an error with
    /// the localized message `unsupported_key`.
    pub fn require(&self, feature_id: u16, unsupported_key: &str) -> Result<u8> {
        match self.feature(feature_id)? {
            Some(index) => Ok(index),
            None => bail!("{}", t!(unsupported_key)),
        }
    }

    /// Reads the whole feature table (including the root feature at index 0).
    pub fn feature_table(&self) -> Result<Vec<FeatureInfo>> {
        let set = self
            .feature(FEATURE_SET_ID)?
            .context("device has no FeatureSet feature")?;
        let count = *self
            .call(set, SET_GET_COUNT, &[])?
            .first()
            .context("empty feature count response")?;
        let mut out = vec![FeatureInfo {
            index: ROOT_INDEX,
            id: ROOT_ID,
            version: 0,
        }];
        for index in 1..=count {
            let r = self.call(set, SET_GET_FEATURE_ID, &[index])?;
            let [id_hi, id_lo, _kind, version, ..] = r.as_slice() else {
                bail!("short feature table entry {index}");
            };
            out.push(FeatureInfo {
                index,
                id: u16::from_be_bytes([*id_hi, *id_lo]),
                version: *version,
            });
        }
        Ok(out)
    }

    /// Stable identity of the physical mouse: its unit ID when it reports
    /// one (same over Bolt and Bluetooth), else the interface and slot.
    pub fn identity(&self) -> String {
        match features::fwinfo::read_unit_id(self) {
            Ok(Some(id)) => format!("unit:{}", id.map(|b| format!("{b:02x}")).concat()),
            r => {
                if let Err(e) = r {
                    log::debug!("cannot read unit id: {e}");
                }
                format!("slot:{}:{}", self.path.to_string_lossy(), self.index)
            }
        }
    }

    /// Replaces the interface name (a receiver's) with the mouse's own name.
    pub fn refine_name(&mut self) {
        match features::name::read(self) {
            Ok(Some(n)) => self.name = n,
            Ok(None) => {}
            Err(e) => log::debug!("cannot read device name: {e}"),
        }
    }
}

/// Well-known feature names for display.
pub fn feature_name(id: u16) -> &'static str {
    match id {
        ROOT_ID => "Root",
        FEATURE_SET_ID => "FeatureSet",
        fwinfo::ID => "DeviceFWVersion",
        name::ID => "DeviceNameType",
        battery::LEGACY_ID => "BatteryStatus",
        battery::ID => "UnifiedBattery",
        host::ID => "ChangeHost",
        controls::ID => "ReprogControlsV4",
        smartshift::LEGACY_ID => "SmartShift",
        smartshift::ID => "SmartShift (enhanced)",
        wheel::ID => "HiResWheel",
        thumbwheel::ID => "ThumbWheel",
        dpi::ID => "AdjustableDPI",
        _ => "",
    }
}

/// `mouse info`: prints the protocol version and feature table.
pub fn print_info() -> Result<()> {
    write_info(&Device::open_default()?, &mut std::io::stdout())
}

/// Writes what `mouse info` shows about `dev` to `out`.
pub fn write_info(dev: &Device, out: &mut dyn std::io::Write) -> Result<()> {
    let (major, minor) = dev.ping()?;
    writeln!(
        out,
        "{}",
        t!(
            "info.header",
            name = dev.name,
            connection = dev.connection.label(),
            index = dev.index,
            major = major,
            minor = minor
        )
    )?;
    let mut full_name = dev.name.clone();
    let line = match features::name::read(dev) {
        Ok(Some(n)) => {
            let line = t!("info.name", name = n);
            full_name = n;
            line
        }
        Ok(None) => t!("info.name_unsupported"),
        Err(e) => t!("info.name_error", error = e),
    };
    writeln!(out, "{line}")?;
    let model = devices::identify(dev.pid, &full_name);
    let line = match model {
        Some(m) => t!(
            "info.model",
            name = m.name,
            min = m.dpi.min,
            max = m.dpi.max,
            buttons = m.buttons.len()
        ),
        None => t!("info.model_unknown"),
    };
    writeln!(out, "{line}")?;
    let line = match features::battery::read(dev) {
        Ok(Some(b)) => {
            let state = match b.charge {
                Charge::Discharging => t!("info.charge.discharging"),
                Charge::Charging => t!("info.charge.charging"),
                Charge::Full => t!("info.charge.full"),
                Charge::Error => t!("info.charge.error"),
                Charge::Other => t!("info.charge.other"),
            };
            let pct = b.percent.map_or("?".to_string(), |p| format!("{p}%"));
            t!("info.battery", percent = pct, state = state)
        }
        Ok(None) => t!("info.battery_unsupported"),
        Err(e) => t!("info.battery_error", error = e),
    };
    writeln!(out, "{line}")?;
    let line = match features::host::read(dev) {
        Ok(Some(h)) => t!("info.hosts", count = h.count, current = h.current as u16 + 1),
        Ok(None) => t!("info.hosts_unsupported"),
        Err(e) => t!("info.hosts_error", error = e),
    };
    writeln!(out, "{line}")?;
    let line = match features::smartshift::read(dev) {
        Ok(Some(s)) => t!("info.smartshift", default = s.default),
        Ok(None) => t!("info.smartshift_unsupported"),
        Err(e) => t!("info.smartshift_error", error = e),
    };
    writeln!(out, "{line}")?;
    if let Some(m) = model {
        for b in m.buttons {
            writeln!(out, "  CID 0x{:04X} {}", b.cid, b.label())?;
        }
    }
    let table = dev.feature_table()?;
    if let Some(m) = model {
        for id in m.features.iter().filter(|id| !table.iter().any(|f| f.id == **id)) {
            writeln!(out, "{}", t!("info.model_missing", id = format!("{id:04X}")))?;
        }
    }
    for f in table {
        writeln!(out, "  [{:02}] 0x{:04X} v{} {}", f.index, f.id, f.version, feature_name(f.id))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hidpp::message::ErrorKind;

    #[test]
    fn slow_call_threshold_is_longer_for_the_root_feature() {
        let ms = Duration::from_millis;
        assert!(!is_slow_call(0x05, ms(300)));
        assert!(is_slow_call(0x05, ms(301)));
        assert!(!is_slow_call(ROOT_INDEX, ms(1000)));
        assert!(is_slow_call(ROOT_INDEX, ms(1001)));
    }

    fn hidpp(version: ErrorKind, code: u8) -> anyhow::Error {
        HidppError { version, feature_index: 0, function: 0, sw_id: 0, code }.into()
    }

    #[test]
    fn classifies_link_failures() {
        assert!(is_link_failure(&Timeout.into()));
        assert!(is_link_failure(&hidpp(ErrorKind::V1, 0x09)));
        let io = anyhow::Error::msg("device gone").context(IoFailed("read"));
        assert!(is_link_failure(&io));
        // The mouse rejected one request, or a setting does not fit: not a link problem.
        assert!(!is_link_failure(&hidpp(ErrorKind::V2, 0x02)));
        assert!(!is_link_failure(&anyhow::anyhow!("dpi out of range")));
    }

    #[test]
    fn software_ids_rotate_and_two_waiting_requests_never_share_one() {
        let mut ids = SwIds::new(DAEMON_SW_IDS);
        let a = ids.take(5, 2);
        let b = ids.take(5, 2);
        assert_ne!(a, b);
        assert!(DAEMON_SW_IDS.contains(&a) && DAEMON_SW_IDS.contains(&b));
        ids.release(5, 2, a);
        ids.release(5, 2, b);
        // Taken in turn, also after release: a late response to a request
        // that timed out does not answer the next one.
        let c = ids.take(5, 2);
        assert_ne!(c, a);
        assert_ne!(c, b);
        ids.release(5, 2, c);
        // Wraps around within the range and skips IDs still waiting.
        let mut ids = SwIds::new(DAEMON_SW_IDS);
        let held = ids.take(1, 1);
        let mut seen = Vec::new();
        for _ in DAEMON_SW_IDS {
            let id = ids.take(1, 1);
            ids.release(1, 1, id);
            seen.push(id);
        }
        assert!(!seen.contains(&held));
        assert!(seen.iter().all(|id| DAEMON_SW_IDS.contains(id)));
    }

    #[test]
    fn daemon_and_client_ids_never_overlap_and_foreign_response_is_rejected() {
        assert!(DAEMON_SW_IDS.start() > CLIENT_SW_IDS.end());
        assert!(!CLIENT_SW_IDS.contains(&0));
        let mut daemon = SwIds::new(DAEMON_SW_IDS);
        let mut client = SwIds::new(CLIENT_SW_IDS);
        for _ in 0..40 {
            let d = daemon.take(0, 0);
            let c = client.take(0, 0);
            assert!(DAEMON_SW_IDS.contains(&d) && CLIENT_SW_IDS.contains(&c));
            let req = Message::request(1, 0, 0, c, &[]).unwrap();
            let foreign = Message::request(1, 0, 0, d, &[]).unwrap();
            assert!(!foreign.answers(&req));
            assert!(req.answers(&req));
            daemon.release(0, 0, d);
            client.release(0, 0, c);
        }
    }

    /// A fake mouse: requests come out of the returned receiver.
    fn fake_mouse() -> (Device, super::super::transport::Feed, std::sync::mpsc::Receiver<Message>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let (dev, feed) = Device::fake(move |raw| {
            tx.send(Message::parse(raw).unwrap()).unwrap();
        });
        (dev, feed, rx)
    }

    /// The mouse's answer to `req` with `params`.
    fn reply(req: &Message, params: &[u8]) -> Vec<u8> {
        let m = Message::request(req.device_index, req.feature_index, req.function, req.sw_id, params);
        m.unwrap().to_bytes()
    }

    /// A button notification of feature `idx` (0x1B04 "buttons held").
    fn button_event(idx: u8, cid: u16) -> Vec<u8> {
        Message::request(INDEX_DIRECT, idx, 0, 0, &cid.to_be_bytes()).unwrap().to_bytes()
    }

    #[test]
    fn a_notification_during_a_call_is_read_at_once() {
        let (dev, feed, requests) = fake_mouse();
        std::thread::scope(|s| {
            let call = s.spawn(|| dev.ping());
            let req = requests.recv_timeout(Duration::from_secs(5)).unwrap();
            // The mouse sends a button while the ping waits for its answer.
            feed.push(&button_event(9, 0x00C3));
            let start = Instant::now();
            let msg = dev.read_message(1_000).unwrap().unwrap();
            assert!(start.elapsed() < Duration::from_millis(500));
            assert_eq!((msg.feature_index, msg.function, msg.sw_id), (9, 0, 0));
            assert!(!call.is_finished(), "the ping still waits");
            feed.push(&reply(&req, &[4, 5, PING_DATA]));
            assert_eq!(call.join().unwrap().unwrap(), (4, 5));
        });
        // The answer went to the ping, not to the notifications.
        assert_eq!(dev.read_message(0).unwrap(), None);
    }

    #[test]
    fn each_call_gets_its_own_response() {
        let (dev, feed, requests) = fake_mouse();
        std::thread::scope(|s| {
            // Same feature and function from two threads (the mouse thread
            // and the check thread), told apart by the software ID.
            let first = s.spawn(|| dev.call(9, 2, &[0x00, 0xC3]));
            let a = requests.recv_timeout(Duration::from_secs(5)).unwrap();
            let second = s.spawn(|| dev.call(9, 2, &[0x00, 0x53]));
            let b = requests.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_ne!(a.sw_id, b.sw_id);
            // Answered in the opposite order; the first one with an error.
            feed.push(&reply(&b, &[0x00, 0x53, 0x01]));
            // HID++ 2.0 error: 0xFF, then the request's feature index,
            // function and software ID, then the error code.
            let failed = (a.function << 4) | a.sw_id;
            let mut err = vec![0x11, INDEX_DIRECT, 0xFF, a.feature_index, failed, 0x02];
            err.resize(20, 0);
            feed.push(&err);
            assert_eq!(&second.join().unwrap().unwrap()[..3], &[0x00, 0x53, 0x01]);
            let e = first.join().unwrap().unwrap_err();
            assert_eq!(e.downcast_ref::<HidppError>().map(|e| e.code), Some(0x02));
        });
        assert_eq!(dev.read_message(0).unwrap(), None);
    }

    #[test]
    fn names_known_features() {
        assert_eq!(feature_name(FEATURE_SET_ID), "FeatureSet");
        assert_eq!(feature_name(0xFFFF), "");
    }

    #[test]
    fn feature_id_is_big_endian() {
        assert_eq!(0x1B04u16.to_be_bytes(), [0x1B, 0x04]);
    }
}
