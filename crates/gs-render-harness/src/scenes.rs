//! The product's own clouds: the live stage and the scanned house, produced
//! by `gs_core::Pipeline` with its default configuration and kept as the
//! wire messages a viewer would receive.
//!
//! Nothing here assumes a sensor resolution, a cell size or a splat count:
//! whatever the pipeline's defaults are is what the renderer checks run on.

use gs_core::codec::{self, Kind};
use gs_core::pipeline::{Outgoing, Pipeline};
use gs_core::protocol::{ControlMsg, PipelineConfig, ScanState, SessionState};
use gs_core::scene::SceneKind;
use gs_render::Viewer;

use crate::dom;

/// A cloud as wire messages, with the session it belongs to.
pub struct SceneCloud {
    /// `Layer` messages in the order they must be applied.
    pub layers: Vec<Vec<u8>>,
    pub state: SessionState,
}

impl SceneCloud {
    fn new(pipeline: &Pipeline) -> Self {
        let layers = layer_messages(pipeline.snapshot());
        Self { layers, state: pipeline.state() }
    }

    /// Hands the cloud to `viewer` the way a transport would.
    pub fn upload(&self, viewer: &Viewer) -> Result<(), String> {
        viewer.clear_layers();
        for message in &self.layers {
            viewer.apply_message(message)?;
        }
        Ok(())
    }
}

fn layer_messages(messages: Vec<Outgoing>) -> Vec<Vec<u8>> {
    messages.into_iter().map(|m| m.bytes).filter(|bytes| codec::peek_kind(bytes) == Ok(Kind::Layer)).collect()
}

/// The pipeline configuration of a scene; `?light=1` selects the light
/// preset (fewer and coarser sensors: faster to build in a browser).
fn config(scene: SceneKind) -> PipelineConfig {
    if dom::query("light").is_some() {
        PipelineConfig::light(scene)
    } else {
        PipelineConfig::for_scene(scene)
    }
}

/// A pipeline for `scene`. `?patch=<JSON object>` is merged into its
/// configuration like a `patch_config` control message, for example
/// `{"fusion":{"enabled":false}}`: what a picture of an older or a heavier
/// configuration needs, without this page knowing the fields.
fn pipeline(scene: SceneKind) -> Result<Pipeline, String> {
    let mut pipeline = Pipeline::new(config(scene));
    if let Some(patch) = dom::query("patch") {
        pipeline.handle_control_json(&format!(r#"{{"type":"patch_config","patch":{patch}}}"#))?;
    }
    Ok(pipeline)
}

/// The live stage `time` seconds into the dance: the background plate in
/// layer 0, the dancer in layer 1.
pub fn stage(time: f32) -> Result<SceneCloud, String> {
    let mut pipeline = stage_pipeline()?;
    pipeline.tick(time);
    Ok(SceneCloud::new(&pipeline))
}

/// A stage pipeline to tick by hand (animated checks).
pub fn stage_pipeline() -> Result<Pipeline, String> {
    pipeline(SceneKind::Stage)
}

/// The layer messages of one tick of `pipeline`.
pub fn tick_layers(pipeline: &mut Pipeline, dt: f32) -> Vec<Vec<u8>> {
    layer_messages(pipeline.tick(dt))
}

/// The house after a complete scan. Yields to the browser while scanning.
pub async fn house() -> Result<SceneCloud, String> {
    let mut pipeline = pipeline(SceneKind::House)?;
    pipeline.handle_control(ControlMsg::ScanFinish);
    let start = dom::now();
    let mut ticks = 0u32;
    while pipeline.scan_state() != Some(ScanState::Done) {
        pipeline.tick(1.0 / 30.0);
        ticks += 1;
        if ticks.is_multiple_of(40) {
            dom::set_status(&format!("scanning the house: {} splats", pipeline.static_splats().len()));
            dom::sleep(0).await;
        }
        if dom::now() - start > 600_000.0 {
            return Err("the house scan did not finish within ten minutes".into());
        }
    }
    web_sys::console::log_1(
        &format!("[scenes] house scanned: {} splats in {:.0} ms", pipeline.static_splats().len(), dom::now() - start).into(),
    );
    Ok(SceneCloud::new(&pipeline))
}
