//! The starting point of the optimisation: the cloud the real-time path
//! fuses from the same capture.
//!
//! The frames go through a `gs_core::ingest::CaptureSession` exactly as an
//! import in the application does, except that the views held out for
//! evaluation are left out, so that they stay unseen. Geometry is metric
//! and in place from the first iteration; the optimiser refines it.

use gs_core::ingest::{CaptureRegion, CaptureSession, IngestParams};
use gs_core::protocol::CaptureMode;
use gs_core::Splat;

use crate::dataset::{Capture, DatasetError};

/// How the starting cloud is made.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct InitParams {
    /// Cell size of the fusion in meters, whatever the capture's mode; `0`
    /// keeps the default of the mode (2.4 cm for a room unless
    /// `room_voxel_size` says otherwise, 8 mm for an object, 1 cm for a
    /// person).
    pub voxel_size: f32,
    /// Cell size for room captures when `voxel_size` is `0`; `0` keeps the
    /// real-time 2.4 cm.
    pub room_voxel_size: f32,
    /// Upper bound on fused cells.
    pub max_voxels: usize,
    /// Multiplies the standard deviations of the fused splats: they are
    /// sized for the viewer's normalised blending and leave gaps when
    /// composited as translucent discs.
    pub splat_scale: f32,
    /// Multiplies the opacity of the fused splats, which are fully opaque:
    /// the optimiser needs room to blend neighbours and to learn.
    pub opacity: f32,
}

impl Default for InitParams {
    fn default() -> Self {
        Self { voxel_size: 0.0, room_voxel_size: 0.0, max_voxels: 3_000_000, splat_scale: 1.4, opacity: 0.7 }
    }
}

/// The fused cloud and what the session learned about the capture.
#[derive(Clone, Debug)]
pub struct Fused {
    /// Splats as viewers hold them: inside the region, floor at `y = 0`.
    pub splats: Vec<Splat>,
    /// Height of the floor in capture coordinates (subtracted from every
    /// `y` on the way to viewers) and whether it is more than a guess.
    pub floor_y: f32,
    pub floor_known: bool,
    /// Region of interest in capture coordinates (object, person).
    pub region: Option<CaptureRegion>,
    pub frames_used: u32,
    /// Cell size the fusion ran with, meters.
    pub voxel_size: f32,
}

/// Splat size, in cells, of the cloud the optimisation starts from. The
/// live path widens its splats to 0.6 to close the slits a real LiDAR leaves
/// on screen; the optimisation learns every size itself and starts better
/// from the tighter cloud (pose registration drifted 1.6 cm from a correct
/// view with 0.6).
const START_KERNEL: f32 = 0.45;

/// The ingest parameters of an offline fusion.
pub fn ingest_params(mode: CaptureMode, init: &InitParams) -> IngestParams {
    let mut params =
        IngestParams { max_voxels: init.max_voxels.max(1000), kernel: START_KERNEL, ..IngestParams::default() };
    if init.voxel_size <= 0.0 && init.room_voxel_size > 0.0 && mode == CaptureMode::Room {
        params.room.voxel_size = init.room_voxel_size;
    }
    if init.voxel_size > 0.0 {
        match mode {
            CaptureMode::Room => params.room.voxel_size = init.voxel_size,
            CaptureMode::Object => params.object.voxel_size = init.voxel_size,
            CaptureMode::Person => params.person.voxel_size = init.voxel_size,
            CaptureMode::Live => params.live.voxel_size = init.voxel_size,
        }
    }
    params.sanitized()
}

/// Fuses the frames `frames` (indices into the capture, ascending) the way
/// the real-time path does. `progress(done, total)` is called now and then
/// and stops the fusion by returning `false`.
pub fn fuse(capture: &Capture, frames: &[usize], init: &InitParams, mut progress: impl FnMut(usize, usize) -> bool) -> Result<Option<Fused>, DatasetError> {
    if !capture.mode.fuses() {
        return Err(DatasetError::Unusable("live captures show a moving scene and cannot be refined".into()));
    }
    let params = ingest_params(capture.mode, init);
    let voxel_size = params.mode(capture.mode).voxel_size;
    let mut session = CaptureSession::from_header(&capture.header, params)?;
    for (done, &i) in frames.iter().enumerate() {
        if done % 16 == 0 && !progress(done, frames.len()) {
            return Ok(None);
        }
        session.handle_decoded(&capture.frame(i)?);
    }
    session.finish();
    let (floor_y, floor_known) = session.floor();
    Ok(Some(Fused { splats: session.static_splats(), floor_y, floor_known, region: session.region(), frames_used: session.frames_used(), voxel_size }))
}
