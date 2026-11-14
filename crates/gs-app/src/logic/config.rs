//! Configuration choices offered by the interface and the `patch_config`
//! objects they send. The pipeline's echo of the configuration is the single
//! source of truth; these helpers only build partial updates.

use gs_core::capture::NoiseModel;
use gs_core::protocol::{ControlMsg, PipelineConfig, ScanState};
use gs_core::rig::RingRig;
use serde_json::{json, Value};

/// A camera resolution the "Capture" section offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolution {
    pub label: &'static str,
    pub width: u32,
    pub height: u32,
}

/// Ring camera resolutions, all 4:3.
pub const RESOLUTIONS: [Resolution; 4] = [
    Resolution { label: "Basse", width: 96, height: 72 },
    Resolution { label: "Moyenne", width: 120, height: 90 },
    Resolution { label: "Haute", width: 160, height: 120 },
    Resolution { label: "Max", width: 240, height: 180 },
];

/// Range of the camera count slider.
pub const CAMERA_COUNT_RANGE: (u32, u32) = (RingRig::MIN_COUNT, 16);
/// Range of the walking speed slider, meters per second.
pub const WALK_SPEED_RANGE: (f32, f32) = (0.3, 2.5);

/// Index into [`RESOLUTIONS`] of the preset the rig currently uses.
pub fn resolution_index(config: &PipelineConfig) -> Option<usize> {
    RESOLUTIONS.iter().position(|r| r.width == config.rig.width && r.height == config.rig.height)
}

pub fn patch_camera_count(count: u32) -> Value {
    json!({ "rig": { "count": count } })
}

pub fn patch_resolution(resolution: Resolution) -> Value {
    json!({ "rig": { "width": resolution.width, "height": resolution.height } })
}

/// Sensor noise on (a consumer depth camera) or off (a perfect sensor).
pub fn patch_noise(on: bool) -> Value {
    let noise = if on { NoiseModel::realistic() } else { NoiseModel::NONE };
    json!({ "noise": noise })
}

pub fn patch_shadows(on: bool) -> Value {
    json!({ "shadows": on })
}

pub fn patch_background_subtraction(on: bool) -> Value {
    json!({ "background_subtraction": on })
}

pub fn patch_walk_speed(speed: f32) -> Value {
    json!({ "scan": { "speed": speed } })
}

/// Recursively merges `patch` into `target`: objects merge key by key,
/// anything else replaces. The same rule the pipeline applies.
pub fn merge(target: &mut Value, patch: Value) {
    match (target, patch) {
        (Value::Object(target), Value::Object(patch)) => {
            for (key, value) in patch {
                merge(target.entry(key).or_insert(Value::Null), value);
            }
        }
        (target, patch) => *target = patch,
    }
}

/// Patches collected while the user drags a slider, sent as one message.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PatchQueue {
    pending: Option<Value>,
}

impl PatchQueue {
    pub fn push(&mut self, patch: Value) {
        match &mut self.pending {
            Some(pending) => merge(pending, patch),
            None => self.pending = Some(patch),
        }
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.pending.is_none()
    }

    /// The message to send, if anything is pending.
    pub fn take(&mut self) -> Option<ControlMsg> {
        self.pending.take().map(|patch| ControlMsg::PatchConfig { patch })
    }
}

/// Label of a scan state.
pub fn scan_label(state: ScanState) -> &'static str {
    match state {
        ScanState::Idle => "Prêt à scanner",
        ScanState::Scanning => "Scan en cours",
        ScanState::Paused => "Scan en pause",
        ScanState::Done => "Scan terminé",
    }
}

/// Identifier of a scan state for the debug hook (the wire name).
pub fn scan_id(state: ScanState) -> &'static str {
    match state {
        ScanState::Idle => "idle",
        ScanState::Scanning => "scanning",
        ScanState::Paused => "paused",
        ScanState::Done => "done",
    }
}

#[cfg(test)]
mod tests {
    use gs_core::scene::SceneKind;

    use super::*;

    /// Applies a patch the way the pipeline does.
    fn apply(config: PipelineConfig, patch: Value) -> PipelineConfig {
        let mut value = serde_json::to_value(config).unwrap();
        merge(&mut value, patch);
        serde_json::from_value::<PipelineConfig>(value).unwrap().sanitized()
    }

    #[test]
    fn every_patch_changes_exactly_its_field() {
        let base = PipelineConfig::light(SceneKind::Stage);

        let c = apply(base, patch_camera_count(9));
        assert_eq!(c, PipelineConfig { rig: RingRig { count: 9, ..base.rig }, ..base });

        let c = apply(base, patch_resolution(RESOLUTIONS[3]));
        assert_eq!(c, PipelineConfig { rig: RingRig { width: 240, height: 180, ..base.rig }, ..base });

        let c = apply(base, patch_noise(true));
        assert_eq!(c, PipelineConfig { noise: NoiseModel::realistic(), ..base });
        assert_eq!(apply(c, patch_noise(false)), base);

        assert_eq!(apply(base, patch_shadows(false)), PipelineConfig { shadows: false, ..base });
        assert_eq!(
            apply(base, patch_background_subtraction(false)),
            PipelineConfig { background_subtraction: false, ..base }
        );

        let c = apply(base, patch_walk_speed(1.5));
        assert_eq!(c.scan.speed, 1.5);
        assert_eq!(PipelineConfig { scan: base.scan, ..c }, base);
    }

    #[test]
    fn presets_are_valid_and_found() {
        for (i, preset) in RESOLUTIONS.iter().enumerate() {
            let c = apply(PipelineConfig::default(), patch_resolution(*preset));
            assert_eq!((c.rig.width, c.rig.height), (preset.width, preset.height), "{}", preset.label);
            assert_eq!(resolution_index(&c), Some(i));
        }
        // Both built-in configurations start on a preset.
        assert!(resolution_index(&PipelineConfig::light(SceneKind::Stage)).is_some());
        assert!(resolution_index(&PipelineConfig::for_scene(SceneKind::Stage)).is_some());
        let odd = PipelineConfig { rig: RingRig { width: 100, ..RingRig::default() }, ..PipelineConfig::default() };
        assert_eq!(resolution_index(&odd), None);
    }

    #[test]
    fn slider_ranges_survive_sanitizing() {
        let base = PipelineConfig::light(SceneKind::House);
        for count in [CAMERA_COUNT_RANGE.0, CAMERA_COUNT_RANGE.1] {
            assert_eq!(apply(base, patch_camera_count(count)).rig.count, count);
        }
        for speed in [WALK_SPEED_RANGE.0, WALK_SPEED_RANGE.1] {
            assert_eq!(apply(base, patch_walk_speed(speed)).scan.speed, speed);
        }
    }

    #[test]
    fn queue_merges_patches_into_one_message() {
        let mut queue = PatchQueue::default();
        assert!(queue.take().is_none());
        queue.push(patch_camera_count(5));
        queue.push(patch_camera_count(7));
        queue.push(patch_resolution(RESOLUTIONS[0]));
        queue.push(patch_shadows(false));
        assert!(!queue.is_empty());
        let Some(ControlMsg::PatchConfig { patch }) = queue.take() else { panic!("expected a patch") };
        assert_eq!(patch, json!({ "rig": { "count": 7, "width": 96, "height": 72 }, "shadows": false }));
        assert!(queue.is_empty());
    }
}
