//! Synthetic mouse button events (CoreGraphics).

use anyhow::{Result, anyhow};
use core_graphics::event::{CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, EventField};

/// Sends a middle (other, button 2) mouse click at the current cursor position.
pub fn middle_click() -> Result<()> {
    let src = super::event_source()?;
    let pos = CGEvent::new(src.clone())
        .map_err(|_| anyhow!("cannot read cursor position"))?
        .location();
    for ty in [CGEventType::OtherMouseDown, CGEventType::OtherMouseUp] {
        let ev = CGEvent::new_mouse_event(src.clone(), ty, pos, CGMouseButton::Center)
            .map_err(|_| anyhow!("cannot create mouse event"))?;
        ev.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, 2);
        ev.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, 1);
        ev.post(CGEventTapLocation::HID);
    }
    Ok(())
}
