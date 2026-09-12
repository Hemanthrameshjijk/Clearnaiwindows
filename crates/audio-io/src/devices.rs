//! Device enumeration helpers, plus VB-Audio Virtual Cable endpoint
//! discovery.
//!
//! Architecture note: this project targets VB-Cable (a third-party virtual
//! audio driver the user installs) rather than shipping a custom Windows
//! audio driver. VB-Cable exposes a render endpoint named "CABLE Input" and
//! a capture endpoint named "CABLE Output" (its default friendly names).
//! The engine renders its processed output *into* "CABLE Input" (so other
//! apps can pick "CABLE Output" as their mic) and can also tap "CABLE
//! Output" directly. `find_vb_cable_endpoints` locates both by name.

/// Case-insensitive substring check for VB-Cable's render endpoint
/// ("CABLE Input (VB-Audio Virtual Cable)" by default). Pure, OS-independent
/// logic, factored out so it can be unit tested without touching WASAPI.
pub fn name_contains_cable_input(friendly_name: &str) -> bool {
    friendly_name.to_ascii_lowercase().contains("cable input")
}

/// Case-insensitive substring check for VB-Cable's capture endpoint
/// ("CABLE Output (VB-Audio Virtual Cable)" by default).
pub fn name_contains_cable_output(friendly_name: &str) -> bool {
    friendly_name.to_ascii_lowercase().contains("cable output")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_default_vb_cable_render_name() {
        assert!(name_contains_cable_input(
            "CABLE Input (VB-Audio Virtual Cable)"
        ));
    }

    #[test]
    fn matches_default_vb_cable_capture_name() {
        assert!(name_contains_cable_output(
            "CABLE Output (VB-Audio Virtual Cable)"
        ));
    }

    #[test]
    fn match_is_case_insensitive() {
        assert!(name_contains_cable_input("cable input (vb-audio virtual cable)"));
        assert!(name_contains_cable_output("Cable OUTPUT (VB-Audio Virtual Cable)"));
    }

    #[test]
    fn does_not_cross_match_input_and_output() {
        assert!(!name_contains_cable_input("CABLE Output (VB-Audio Virtual Cable)"));
        assert!(!name_contains_cable_output("CABLE Input (VB-Audio Virtual Cable)"));
    }

    #[test]
    fn unrelated_device_names_do_not_match() {
        for name in [
            "Realtek(R) Audio",
            "Microphone (USB Audio Device)",
            "Speakers (High Definition Audio Device)",
            "Headset Microphone (Bluetooth)",
            "",
        ] {
            assert!(!name_contains_cable_input(name), "false positive on {name:?}");
            assert!(!name_contains_cable_output(name), "false positive on {name:?}");
        }
    }

    #[test]
    fn substring_can_appear_anywhere_in_the_name() {
        // Some locales/OEMs prefix or suffix the friendly name; we only
        // require the substring, not an exact/anchored match.
        assert!(name_contains_cable_input("Line (2- CABLE Input (VB-Audio Virtual Cable))"));
    }
}

#[cfg(windows)]
mod wasapi_impl {
    use anyhow::{Context, Result};
    use wasapi::{Device, DeviceEnumerator, Direction};

    /// (device id, friendly name) pairs for every active device in the
    /// given direction.
    fn list_devices(direction: Direction) -> Result<Vec<(String, String)>> {
        let enumerator = DeviceEnumerator::new().context("creating WASAPI device enumerator")?;
        let collection = enumerator
            .get_device_collection(&direction)
            .with_context(|| format!("enumerating {direction:?} devices"))?;

        let mut out = Vec::new();
        for device in &collection {
            let device = device.context("reading a device entry from the device collection")?;
            let id = device.get_id().context("reading device id")?;
            let name = device
                .get_friendlyname()
                .context("reading device friendly name")?;
            out.push((id, name));
        }
        Ok(out)
    }

    /// Lists (id, friendly name) for every active capture (microphone-side)
    /// endpoint.
    pub fn list_capture_devices() -> Result<Vec<(String, String)>> {
        list_devices(Direction::Capture)
    }

    /// Lists (id, friendly name) for every active render (speaker-side)
    /// endpoint.
    pub fn list_render_devices() -> Result<Vec<(String, String)>> {
        list_devices(Direction::Render)
    }

    /// Opens a specific capture device by its enumerated device id (as
    /// returned by `list_capture_devices`), not just "the current default".
    pub fn open_capture_device_by_id(device_id: &str) -> Result<Device> {
        DeviceEnumerator::new()
            .context("creating WASAPI device enumerator")?
            .get_device(device_id)
            .with_context(|| format!("opening capture device by id '{device_id}'"))
    }

    /// Opens a specific render device by its enumerated device id. This is
    /// how the virtual-mic pipeline renders *into* "CABLE Input" specifically
    /// rather than the user's real default speaker.
    pub fn open_render_device_by_id(device_id: &str) -> Result<Device> {
        DeviceEnumerator::new()
            .context("creating WASAPI device enumerator")?
            .get_device(device_id)
            .with_context(|| format!("opening render device by id '{device_id}'"))
    }

    /// Finds the first capture device whose friendly name contains
    /// `name_substr` (case-insensitive).
    pub fn find_capture_device_by_name(name_substr: &str) -> Result<Option<Device>> {
        find_device_by_name(Direction::Capture, name_substr)
    }

    /// Finds the first render device whose friendly name contains
    /// `name_substr` (case-insensitive).
    pub fn find_render_device_by_name(name_substr: &str) -> Result<Option<Device>> {
        find_device_by_name(Direction::Render, name_substr)
    }

    fn find_device_by_name(direction: Direction, name_substr: &str) -> Result<Option<Device>> {
        let enumerator = DeviceEnumerator::new().context("creating WASAPI device enumerator")?;
        let collection = enumerator
            .get_device_collection(&direction)
            .with_context(|| format!("enumerating {direction:?} devices"))?;

        let wanted = name_substr.to_ascii_lowercase();
        for device in &collection {
            let device = device.context("reading a device entry from the device collection")?;
            let name = device
                .get_friendlyname()
                .context("reading device friendly name")?;
            if name.to_ascii_lowercase().contains(&wanted) {
                return Ok(Some(device));
            }
        }
        Ok(None)
    }

    /// Scans render/capture endpoints for VB-Audio Virtual Cable's default
    /// endpoint names and opens whichever ones are present.
    ///
    /// Returns `(render_device, capture_device)`:
    /// - `render_device` is "CABLE Input" - what the engine renders into so
    ///   other apps can pick "CABLE Output" as their virtual microphone.
    /// - `capture_device` is "CABLE Output" - useful if the engine itself
    ///   needs to tap what's flowing through the cable.
    ///
    /// Either side can legitimately be `None` if VB-Cable isn't installed or
    /// the endpoint is currently disabled; that is not treated as an error
    /// here; only real WASAPI/enumeration failures are.
    pub fn find_vb_cable_endpoints() -> Result<(Option<Device>, Option<Device>)> {
        let render_id = list_render_devices()?
            .into_iter()
            .find(|(_, name)| super::name_contains_cable_input(name))
            .map(|(id, _)| id);
        let capture_id = list_capture_devices()?
            .into_iter()
            .find(|(_, name)| super::name_contains_cable_output(name))
            .map(|(id, _)| id);

        let render_device = render_id.as_deref().map(open_render_device_by_id).transpose()?;
        let capture_device = capture_id.as_deref().map(open_capture_device_by_id).transpose()?;

        Ok((render_device, capture_device))
    }
}

#[cfg(windows)]
pub use wasapi_impl::*;
