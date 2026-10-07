//! Colour-managed canvas display: the document's composite shown through document profile →
//! monitor profile, on the GPU canvas (folded into the display 3D LUT) and the CPU canvas
//! (an 8-bit transform of the composite).
//!
//! * The composite is in [`composite_profile`] (the document profile for RGB, its gray curve as
//!   RGB for gray documents, sRGB for CMYK — read through the document's CMYK profile, see
//!   `photocraft_color::convert::CmykSpace` — and Lab).
//! * Linear composites (EXR/HDR, linear profiles) are stored in the 8-bit canvas texture
//!   sRGB-encoded ([`CanvasDisplay::encode_srgb`]) so shadows keep their precision; the display
//!   source profile is then the same primaries with the sRGB curve.
//! * The monitor profile comes from Edit › Color Settings › Monitor Profile: `auto` (the
//!   platform's main display profile when the app supplied one, else sRGB), a built-in id or
//!   an `.icc` path.
//! * When the source and the monitor match (sRGB documents on an sRGB display, the common
//!   case) the transform is the identity and nothing is applied on either path.
//!
//! Results are cached per (document profile, mode, monitor profile); the rendering intent is
//! relative colorimetric with black point compensation, as for Photoshop's monitor display.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use photocraft_cms::{Builtin, ColorSpace, Curve, Intent, Profile, Transform};
use photocraft_compose::Buffer;
use photocraft_doc::Document;
use photocraft_raster::Rgba8Image;

use crate::color_cmds::{ColorState, composite_profile, mode_space, profile_from_bytes, resolve_profile};
use crate::{EngineError, Result};

/// Display intent and black point compensation of the monitor transform.
pub const DISPLAY_INTENT: Intent = Intent::RelativeColorimetric;
pub const DISPLAY_BPC: bool = true;

/// How the canvas shows one document.
#[derive(Debug)]
pub struct CanvasDisplay {
    /// The canvas texture stores `srgb_encode(value)` instead of the composite value (linear
    /// composites; see the module docs).
    pub encode_srgb: bool,
    /// Profile of the canvas texture values (the composite profile, or its sRGB-curve twin
    /// when `encode_srgb`).
    pub source: Arc<Profile>,
    /// The monitor profile.
    pub monitor: Arc<Profile>,
    /// Texture values → monitor; `None` when that is the identity (within half an 8-bit step).
    pub transform: Option<Arc<Transform>>,
    /// Changes whenever any of the above changes (for UI caches).
    pub key: u64,
}

impl CanvasDisplay {
    /// Nothing to do: values go to the screen unchanged.
    pub fn is_identity(&self) -> bool {
        !self.encode_srgb && self.transform.is_none()
    }

    /// CPU canvas: a straight-alpha composite → RGBA8 monitor values.
    pub fn to_rgba8(&self, buf: &Buffer) -> Rgba8Image {
        let mut img = if self.encode_srgb { encode_rgba8(buf) } else { buf.to_rgba8() };
        if let Some(t) = &self.transform {
            apply_u8(t, &mut img.pixels);
        }
        img
    }

    /// GPU canvas: a CPU composite as the texture stores it (sRGB-encoded for linear
    /// composites; the display LUT does the rest). Values above 1.0 are encoded too (the sRGB
    /// curve extended), for the float canvas texture of 32-bit documents.
    pub fn texture_buffer<'a>(&self, buf: &'a Buffer) -> Cow<'a, Buffer> {
        if !self.encode_srgb {
            return Cow::Borrowed(buf);
        }
        let mut b = buf.clone();
        for p in &mut b.px {
            for v in &mut p[..3] {
                // Capped at the largest half float; NaN is dropped by the texel conversion.
                *v = photocraft_color::convert::linear_to_srgb(v.clamp(0.0, 65504.0));
            }
        }
        Cow::Owned(b)
    }
}

/// Linear → sRGB-encoded 8-bit codes, indexed by the linear value in 1/65535 steps.
fn encode_table() -> &'static [u8] {
    static T: OnceLock<Vec<u8>> = OnceLock::new();
    T.get_or_init(|| (0..=65535u32).map(|i| (photocraft_color::convert::linear_to_srgb(i as f32 / 65535.0) * 255.0 + 0.5) as u8).collect())
}

fn encode_rgba8(buf: &Buffer) -> Rgba8Image {
    let t = encode_table();
    let mut img = Rgba8Image::new(buf.rect.width(), buf.rect.height());
    let code = |v: f32| t.get((v.clamp(0.0, 1.0) * 65535.0 + 0.5) as usize).copied().unwrap_or(255);
    for (o, p) in img.pixels.as_chunks_mut::<4>().0.iter_mut().zip(&buf.px) {
        *o = [code(p[0]), code(p[1]), code(p[2]), (p[3].clamp(0.0, 1.0) * 255.0 + 0.5) as u8];
    }
    img
}

/// In-place 8-bit RGBA transform (alpha kept), in blocks so the scratch copy stays small.
fn apply_u8(t: &Transform, px: &mut [u8]) {
    const BLOCK: usize = 1 << 20;
    let mut src = Vec::new();
    for chunk in px.chunks_mut(BLOCK * 4) {
        src.clear();
        src.extend_from_slice(chunk);
        t.convert_u8(&src, 4, chunk, 4, true);
    }
}

/// Is `p` an RGB matrix/TRC profile with linear curves?
fn is_linear_rgb(p: &Profile) -> bool {
    p.color_space == ColorSpace::Rgb && p.is_matrix_shaper() && p.trc.as_ref().is_some_and(|t| t.iter().all(Curve::is_identity))
}

/// The same primaries with the sRGB curve (what an sRGB-encoded texture of a linear composite is in).
fn srgb_curve_twin(p: &Profile) -> Profile {
    let c = photocraft_cms::curve::srgb_trc();
    let mut q = p.clone();
    q.trc = Some([c.clone(), c.clone(), c]);
    q.description = format!("{} (sRGB-encoded)", p.description);
    q.with_encoded_bytes()
}

/// Does `t` map every lattice point (the neutral axis only for gray documents) to itself
/// within half an 8-bit step?
fn is_identity(t: &Transform, neutral_only: bool) -> bool {
    const N: usize = 9;
    let s = (N - 1) as f32;
    let mut out = [0.0f32; 16];
    let mut check = |v: [f32; 3]| {
        t.eval(&v, &mut out);
        (0..3).all(|i| (out[i] - v[i]).abs() <= 0.5 / 255.0)
    };
    if neutral_only {
        return (0..=32).all(|i| check([i as f32 / 32.0; 3]));
    }
    (0..N).all(|b| (0..N).all(|g| (0..N).all(|r| check([r as f32 / s, g as f32 / s, b as f32 / s]))))
}

fn hash_of(v: impl std::hash::Hash) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

/// What the platform reported about the display's profile (Monitor Profile = `auto`; #569).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum MonitorDetection {
    /// No platform reader (web, Linux, Windows, tests).
    #[default]
    Unsupported,
    /// Still being read.
    Pending,
    /// The reader ran but returned no usable profile.
    Failed { reason: String },
    /// Profile bytes (in [`ColorState::monitor_profile`]) read for this display.
    Found { display: String },
}

/// A display profile read by the platform.
#[derive(Clone, Debug, PartialEq)]
pub struct DetectedMonitor {
    /// The display the profile belongs to (e.g. "Built-in Retina Display").
    pub display: String,
    pub icc: Vec<u8>,
}

/// The monitor profile the canvas is actually shown in, and why (`edit.colorSettings`'s
/// `monitorStatus`, Help › System Info).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorStatus {
    /// Color Settings › Monitor Profile as set: `auto`, a built-in id or an `.icc` path.
    pub requested: String,
    /// `auto` (the display's own profile), `manual` (the chosen profile) or `fallback` (sRGB,
    /// because the requested profile isn't available or usable: see `reason`).
    pub source: &'static str,
    /// Description of the profile in use.
    pub profile: String,
    /// Its content hash, to tell profiles with the same name apart.
    pub fingerprint: String,
    pub detection: MonitorDetection,
    pub reason: Option<String>,
}

impl MonitorStatus {
    /// One line for Help › System Info and the Color Settings dialog.
    pub fn summary(&self) -> String {
        let display = match &self.detection {
            MonitorDetection::Found { display } if self.source == "auto" => format!(" for {display}"),
            _ => String::new(),
        };
        match &self.reason {
            Some(r) => format!("{} ({}: {r})", self.profile, self.source),
            None => format!("{} ({}{display})", self.profile, self.source),
        }
    }
}

/// The resolved monitor profile and what it was resolved from (setting, platform bytes,
/// detection state).
type MonitorCache = Option<(String, Option<Arc<Vec<u8>>>, MonitorDetection, Arc<Profile>, MonitorStatus)>;

/// Caches of [`ColorState`] for the display (monitor profile and per-document displays).
#[derive(Default)]
pub struct DisplayCaches {
    monitor: Mutex<MonitorCache>,
    canvas: Mutex<HashMap<(ColorSpace, u64, u64), Arc<CanvasDisplay>>>,
}

impl ColorState {
    /// The monitor profile: Color Settings › Monitor Profile (`auto`: the platform's profile when
    /// supplied in [`ColorState::monitor_profile`], else sRGB). Profiles that are missing,
    /// unreadable, not RGB or unusable as a display destination fall back to sRGB, and
    /// [`ColorState::monitor_status`] says so.
    pub fn monitor(&self) -> Arc<Profile> {
        self.resolved_monitor().0
    }

    /// What [`ColorState::monitor`] resolved to, and why.
    pub fn monitor_status(&self) -> MonitorStatus {
        self.resolved_monitor().1
    }

    /// Record the platform's display profile reading (`auto`).
    pub fn set_detected_monitor(&mut self, r: std::result::Result<DetectedMonitor, String>) {
        match r {
            Ok(m) => {
                self.monitor_profile = Some(Arc::new(m.icc));
                self.monitor_detection = MonitorDetection::Found { display: m.display };
            }
            Err(reason) => {
                self.monitor_profile = None;
                self.monitor_detection = MonitorDetection::Failed { reason };
            }
        }
    }

    fn resolved_monitor(&self) -> (Arc<Profile>, MonitorStatus) {
        let spec = self.settings.monitor_profile.as_str();
        let mut cache = self.display.monitor.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((s, b, d, p, st)) = cache.as_ref()
            && s == spec
            && *d == self.monitor_detection
            && match (b, &self.monitor_profile) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                _ => false,
            }
        {
            return (p.clone(), st.clone());
        }
        let auto = spec.is_empty() || spec == "auto";
        let found = if auto {
            match (&self.monitor_profile, &self.monitor_detection) {
                (Some(b), _) => profile_from_bytes(b).map_err(|e| format!("the display profile can't be read: {e}")),
                (None, MonitorDetection::Pending) => Err("the display profile hasn't been read yet".into()),
                (None, MonitorDetection::Failed { reason }) => Err(reason.clone()),
                (None, _) => Err("this platform doesn't report display profiles".into()),
            }
        } else {
            resolve_profile(spec, None, Some(photocraft_color::ColorMode::Rgb)).map_err(|e| e.to_string())
        };
        let usable = found.and_then(|p| {
            if p.color_space != ColorSpace::Rgb {
                return Err(format!("`{}` is a {:?} profile, not RGB", p.description, p.color_space));
            }
            // The canvas transforms end at this profile: one that can't be a destination would
            // otherwise leave the canvas silently unmanaged.
            Transform::new(Builtin::Srgb.profile(), &p, DISPLAY_INTENT, DISPLAY_BPC)
                .map_err(|e| format!("`{}` can't be used as a display profile: {e}", p.description))?;
            Ok(p)
        });
        let (p, source, reason) = match usable {
            Ok(p) => (p, if auto { "auto" } else { "manual" }, None),
            Err(r) => (Arc::new(Builtin::Srgb.profile().clone()), "fallback", Some(r)),
        };
        let st = MonitorStatus {
            requested: if spec.is_empty() { "auto".into() } else { spec.to_string() },
            source,
            profile: p.description.clone(),
            fingerprint: format!("{:016x}", p.content_hash()),
            detection: self.monitor_detection.clone(),
            reason,
        };
        *cache = Some((spec.to_string(), self.monitor_profile.clone(), self.monitor_detection.clone(), p.clone(), st.clone()));
        (p, st)
    }

    /// How the canvas shows `doc` (cached per document profile, mode and monitor profile).
    pub fn canvas_display(&self, doc: &Document) -> Result<Arc<CanvasDisplay>> {
        let space = mode_space(doc.mode);
        let doc_hash = doc.icc_profile.as_ref().and_then(|b| profile_from_bytes(b).ok()).filter(|p| p.color_space == space).map_or(0, |p| p.content_hash());
        let monitor = self.monitor();
        let key = (space, doc_hash, monitor.content_hash());
        if let Some(d) = self.display.canvas.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
            return Ok(d.clone());
        }
        let composite = composite_profile(doc);
        let encode_srgb = is_linear_rgb(&composite);
        let source = if encode_srgb { Arc::new(srgb_curve_twin(&composite)) } else { composite };
        let transform = if source.content_hash() == monitor.content_hash() {
            None
        } else {
            let t = Transform::new(&source, &monitor, DISPLAY_INTENT, DISPLAY_BPC).map_err(|e| EngineError::Other(format!("colour management: {e}")))?;
            (!is_identity(&t, space == ColorSpace::Gray)).then(|| Arc::new(t))
        };
        let d = Arc::new(CanvasDisplay {
            encode_srgb,
            key: hash_of((source.content_hash(), monitor.content_hash(), encode_srgb, transform.is_some())),
            source,
            monitor,
            transform,
        });
        let mut c = self.display.canvas.lock().unwrap_or_else(|e| e.into_inner());
        if c.len() > 32 {
            c.clear();
        }
        c.insert(key, d.clone());
        Ok(d)
    }

    /// Changes whenever the canvas display of `doc` changes: profiles, monitor, Proof Colors,
    /// Gamut Warning and 32-bit preview settings (for the UI's LUT cache).
    pub fn display_signature(&self, doc: &Document) -> u64 {
        let cm = self.canvas_display(doc).map(|d| d.key).unwrap_or(0);
        // Per frame: read the proof state in place (the default state allocates a profile).
        let proof = self.proof_ref(doc.id).filter(|pv| pv.enabled || pv.gamut_warning).map(|pv| {
            let s = &pv.setup;
            (pv.enabled, pv.gamut_warning, s.profile.content_hash(), s.intent, s.bpc, s.simulate_paper, s.kind, pv.gamut_threshold.to_bits())
        });
        let hdr =
            crate::proof_sim::hdr_active(self, doc).then(|| self.hdr.get(&doc.id).map(|h| (h.highlight_compression, h.exposure.to_bits(), h.gamma.to_bits())));
        hash_of((cm, proof, hdr))
    }
}

#[cfg(test)]
#[path = "display_color_tests.rs"]
mod tests;
