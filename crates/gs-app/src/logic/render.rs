//! The rendering choice of the interface: which blending the viewer is
//! asked for, the splat size that suits what it really draws, and what to
//! tell the user when it had to fall back.

use gs_core::scene::SceneKind;
use gs_render::{ActiveBlend, BlendMode, BlendQuality, BlendReason, BlendStatus};

/// What the "Rendu" control offers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BlendChoice {
    /// By content, soft silhouettes on a screen and hard ones in a headset.
    #[default]
    Auto,
    /// Normalised blending with soft silhouettes.
    Soft,
    /// Normalised blending with hard silhouettes (about two thirds of the
    /// work of the soft level).
    Hard,
    /// Sorted alpha blending, the fallback of GPUs without float targets.
    Sorted,
}

impl BlendChoice {
    /// In the order of the control.
    pub const ALL: [Self; 4] = [Self::Auto, Self::Soft, Self::Hard, Self::Sorted];
    pub const LABELS: [&'static str; 4] = ["Auto", "Doux", "Net", "Trié"];

    /// Identifier for the stored settings and the debug hook.
    pub fn id(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Soft => "soft",
            Self::Hard => "hard",
            Self::Sorted => "sorted",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|choice| choice.id() == id)
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|c| *c == self).unwrap_or(0)
    }

    /// What the viewer is asked for. In a headset the automatic choice
    /// takes the hard level: a headset GPU draws every eye at a high
    /// resolution, and the soft level costs half as much again. Nobody has
    /// measured a real headset yet; whoever owns one can pick "Doux".
    pub fn params(self, in_vr: bool) -> (BlendMode, BlendQuality) {
        match self {
            Self::Auto if in_vr => (BlendMode::Auto, BlendQuality::Hard),
            Self::Auto => (BlendMode::Auto, BlendQuality::Soft),
            Self::Soft => (BlendMode::Normalized, BlendQuality::Soft),
            Self::Hard => (BlendMode::Normalized, BlendQuality::Hard),
            Self::Sorted => (BlendMode::Sorted, BlendQuality::Soft),
        }
    }
}

/// The splat scale that scored best against ground truth for a scene in
/// the blending mode the viewer really uses (the quality round's
/// measurements): 1.0 on the stage in every mode; in the house 0.8 with
/// soft silhouettes and 1.0 with hard ones. In the sorted mode the viewer
/// widens surfels by itself (`RenderParams::sorted_surfel_scale`), so the
/// slider stays at 1.
pub fn recommended_splat_scale(scene: SceneKind, active: ActiveBlend) -> f32 {
    match (scene, active) {
        (SceneKind::House, ActiveBlend::NormalizedSoft) => 0.8,
        _ => 1.0,
    }
}

/// Short name of the blending a frame is drawn with.
pub fn active_label(active: ActiveBlend) -> &'static str {
    match active {
        ActiveBlend::Sorted => "trié",
        ActiveBlend::NormalizedHard => "normalisé, contours nets",
        ActiveBlend::NormalizedSoft => "normalisé, contours doux",
    }
}

/// What to tell the user when the viewer wanted the normalised mode and
/// draws sorted instead; `None` when nothing fell back.
pub fn fallback_text(status: BlendStatus) -> Option<&'static str> {
    match status.reason {
        BlendReason::NoFloatTargets => Some(
            "Rendu de secours\u{a0}: cette carte graphique ne sait pas dessiner dans des textures à virgule flottante, les splats sont donc triés et mélangés par transparence.",
        ),
        BlendReason::TargetsFailed => Some(
            "Rendu de secours\u{a0}: la mémoire graphique a manqué pour le rendu normalisé, les splats sont triés. Un nouvel essai a lieu dès que la fenêtre change de taille.",
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_round_trip_and_map_to_viewer_parameters() {
        for (i, choice) in BlendChoice::ALL.into_iter().enumerate() {
            assert_eq!(BlendChoice::from_id(choice.id()), Some(choice));
            assert_eq!(choice.index(), i);
        }
        assert_eq!(BlendChoice::from_id("bogus"), None);
        assert_eq!(BlendChoice::LABELS.len(), BlendChoice::ALL.len());
        assert_eq!(BlendChoice::Auto.params(false), (BlendMode::Auto, BlendQuality::Soft));
        // Hard by default inside a headset, and only by default.
        assert_eq!(BlendChoice::Auto.params(true), (BlendMode::Auto, BlendQuality::Hard));
        for in_vr in [false, true] {
            assert_eq!(BlendChoice::Soft.params(in_vr), (BlendMode::Normalized, BlendQuality::Soft));
            assert_eq!(BlendChoice::Hard.params(in_vr), (BlendMode::Normalized, BlendQuality::Hard));
            assert_eq!(BlendChoice::Sorted.params(in_vr).0, BlendMode::Sorted);
        }
    }

    #[test]
    fn recommended_scales_are_the_measured_ones() {
        for active in [ActiveBlend::Sorted, ActiveBlend::NormalizedHard, ActiveBlend::NormalizedSoft] {
            assert_eq!(recommended_splat_scale(SceneKind::Stage, active), 1.0);
        }
        assert_eq!(recommended_splat_scale(SceneKind::House, ActiveBlend::NormalizedSoft), 0.8);
        assert_eq!(recommended_splat_scale(SceneKind::House, ActiveBlend::NormalizedHard), 1.0);
        assert_eq!(recommended_splat_scale(SceneKind::House, ActiveBlend::Sorted), 1.0);
    }

    #[test]
    fn only_fallbacks_are_announced() {
        let status = |reason| BlendStatus { active: ActiveBlend::Sorted, reason };
        assert!(fallback_text(status(BlendReason::NoFloatTargets)).is_some());
        assert!(fallback_text(status(BlendReason::TargetsFailed)).is_some());
        for reason in [BlendReason::Requested, BlendReason::SurfelContent, BlendReason::ImportedContent] {
            assert!(fallback_text(status(reason)).is_none());
        }
        for text in [fallback_text(status(BlendReason::NoFloatTargets)), fallback_text(status(BlendReason::TargetsFailed))] {
            assert!(!text.unwrap().contains('\u{2014}'));
        }
    }
}
