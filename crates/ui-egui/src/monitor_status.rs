//! The monitor profile in the shell (#569): reads the displays and their profiles through the
//! platform (`Services::read_displays`) at launch and again whenever they may have changed,
//! picks the display each window is on (its canvas is then shown in that display's profile),
//! says when the canvas falls back to sRGB, and gives Help › System Info its lines.
//!
//! A re-read is started when the app comes back to the front (a profile changed in System
//! Settings or by a calibration tool meanwhile), when a window is on no known display or the
//! display it is on doesn't have the size the reading said (a display connected, removed,
//! rearranged or set to another resolution). At most one read runs at a time, at most one starts
//! every [`REREAD_SECS`].

use std::sync::mpsc::Receiver;

use photocraft_engine::display_color::{Display, MonitorDetection, display_at};

use crate::PhotocraftApp;

/// A platform reading: the displays, or why there are none.
pub type Detection = Result<Vec<Display>, String>;

/// Starts a reading in the background (`None`: no reader on this platform).
pub type ReadDisplaysFn = std::sync::Arc<dyn Fn() -> Option<Receiver<Detection>> + Send + Sync>;

/// Notice title when a display's profile can't be used.
pub const FALLBACK_TITLE: &str = "Display profile not used";

/// Minimum time between two readings started by the triggers above.
pub const REREAD_SECS: f64 = 5.0;

/// The shell's reading state (`PhotocraftApp::monitors`).
#[derive(Default)]
pub struct State {
    pending: Option<Receiver<Detection>>,
    /// Another reading is wanted once the pending one is done or the interval has passed.
    again: bool,
    /// When the last reading started (egui time).
    started: Option<f64>,
    focused: Option<bool>,
    /// Fallbacks already reported (display id, reason), so each is told once.
    reported: Vec<(u32, String)>,
}

/// A reading started before the shell existed (the desktop app starts one at launch and waits
/// for it briefly so the first frames already use the right profile): applied when it arrives.
pub fn pending(app: &mut PhotocraftApp, rx: Receiver<Detection>) {
    if app.session.color.displays.is_empty() {
        app.session.color.monitor_detection = MonitorDetection::Pending;
    }
    app.monitors.pending = Some(rx);
}

/// Per-frame (main window): apply a reading that arrived, track the main window's display, and
/// start a new reading when the displays may have changed.
pub fn poll(app: &mut PhotocraftApp, ctx: &egui::Context) {
    if let Some(rx) = &app.monitors.pending {
        let r = match rx.try_recv() {
            Ok(r) => Some(r),
            // No input may come for a while: keep a frame scheduled to pick it up.
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err("the display profile reader stopped without an answer".into())),
        };
        match r {
            Some(r) => {
                app.monitors.pending = None;
                apply(app, r);
                ctx.request_repaint();
            }
            None => ctx.request_repaint_after(std::time::Duration::from_millis(250)),
        }
    }
    let main = view_display(app, ctx);
    if main != app.session.color.main_display {
        app.session.color.main_display = main;
        ctx.request_repaint();
    }
    let focused = ctx.input(|i| i.focused);
    let regained = focused && app.monitors.focused == Some(false);
    app.monitors.focused = Some(focused);
    if regained || displays_changed(app, ctx) {
        app.monitors.again = true;
    }
    if app.monitors.again && app.monitors.pending.is_none() {
        let now = ctx.input(|i| i.time);
        let wait = app.monitors.started.map_or(0.0, |t| t + REREAD_SECS - now);
        if wait <= 0.0 {
            start(app, now);
        } else {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(wait));
        }
    }
}

fn start(app: &mut PhotocraftApp, now: f64) {
    app.monitors.again = false;
    let Some(read) = app.services.read_displays.clone() else { return };
    app.monitors.started = Some(now);
    app.monitors.pending = read();
}

/// Record a reading. Displays whose profile can't be used leave their windows on sRGB: say so
/// once per display and reason (an unusable profile must not look like working colour
/// management). Manual monitor profiles don't depend on the reading.
pub fn apply(app: &mut PhotocraftApp, r: Detection) {
    if let Some(e) = app.session.color.set_displays(r) {
        log::warn!("re-reading the display profiles failed: {e}; keeping the previous ones");
    }
    let c = &app.session.color;
    let fallbacks: Vec<(u32, String, String)> = if c.displays.is_empty() {
        let st = c.monitor_status();
        match (st.source, st.reason, &st.detection) {
            ("fallback", Some(r), MonitorDetection::Failed { .. }) => vec![(0, String::new(), r)],
            _ => vec![],
        }
    } else {
        c.displays
            .iter()
            .filter_map(|d| {
                let st = c.monitor_status_for(Some(d.id));
                (st.source == "fallback" && st.requested == "auto").then(|| (d.id, d.name.clone(), st.reason.unwrap_or_default()))
            })
            .collect()
    };
    for (id, name, reason) in fallbacks {
        if app.monitors.reported.iter().any(|(i, r)| *i == id && *r == reason) {
            continue;
        }
        app.monitors.reported.push((id, reason.clone()));
        let on = if name.is_empty() { String::new() } else { format!(" on {name}") };
        let lines = vec![reason, format!("The canvas{on} is shown as sRGB. Choose a monitor profile in Edit › Color Settings.")];
        crate::notices::post(app, FALLBACK_TITLE, lines, true);
    }
}

/// The frame of the window `ctx` draws, in OS points (as `Display::frame`). egui reports it in
/// its own points, which include the UI zoom.
fn window_frame(ctx: &egui::Context) -> Option<[f64; 4]> {
    let (r, native) = ctx.input(|i| (i.viewport().outer_rect, i.viewport().native_pixels_per_point));
    let r = r?;
    let f = f64::from(ctx.pixels_per_point() / native.filter(|n| *n > 0.0)?);
    let v = [f64::from(r.min.x) * f, f64::from(r.min.y) * f, f64::from(r.width()) * f, f64::from(r.height()) * f];
    v.iter().all(|x| x.is_finite()).then_some(v)
}

/// The display showing most of the window `ctx` draws (`None`: unknown, e.g. no displays read
/// or no window position, as on Wayland).
pub fn view_display(app: &PhotocraftApp, ctx: &egui::Context) -> Option<u32> {
    display_at(&app.session.color.displays, window_frame(ctx)?)
}

/// The window is on no display we know, or on one whose size differs from what was read.
fn displays_changed(app: &PhotocraftApp, ctx: &egui::Context) -> bool {
    let c = &app.session.color;
    if c.displays.is_empty() || !matches!(c.monitor_detection, MonitorDetection::Found) {
        return false;
    }
    let Some(frame) = window_frame(ctx) else { return false };
    let Some(d) = display_at(&c.displays, frame).and_then(|id| c.displays.iter().find(|d| d.id == id)) else { return true };
    // egui's monitor size is the window's display (winit's rule, AppKit's on macOS).
    let size = ctx.input(|i| i.viewport().monitor_size);
    let native = ctx.input(|i| i.viewport().native_pixels_per_point).filter(|n| *n > 0.0);
    match (size, native) {
        (Some(s), Some(n)) => {
            let f = f64::from(ctx.pixels_per_point() / n);
            (f64::from(s.x) * f - d.frame[2]).abs() > 2.0 || (f64::from(s.y) * f - d.frame[3]).abs() > 2.0
        }
        _ => false,
    }
}

/// Help › System Info's lines: the monitor profile of each display.
pub fn summary_lines(app: &PhotocraftApp) -> Vec<String> {
    let c = &app.session.color;
    if c.displays.is_empty() {
        return vec![format!("Monitor profile: {}", c.monitor_status().summary())];
    }
    c.displays
        .iter()
        .map(|d| {
            let main = if c.main_display == Some(d.id) { " (main window)" } else { "" };
            format!("Monitor profile on {}{main}: {}", d.name, c.monitor_status_for(Some(d.id)).summary())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_engine::Session;
    use std::sync::Arc;

    fn app() -> PhotocraftApp {
        PhotocraftApp::new(Session::new(), Default::default())
    }

    fn p3() -> Option<Arc<Vec<u8>>> {
        Some(photocraft_engine::color_cmds::resolve_profile("display-p3", None, None).unwrap().to_bytes())
    }

    /// The #569 setup: built-in display (primary) without a usable profile here, a P3 one to its
    /// right and higher up.
    fn displays(builtin: Option<Arc<Vec<u8>>>) -> Vec<Display> {
        vec![
            Display { id: 1, name: "Built-in Retina Display".into(), frame: [0.0, 0.0, 1728.0, 1117.0], profile_name: Some("Color LCD".into()), icc: builtin },
            Display { id: 4, name: "ROG PG32UQX".into(), frame: [1728.0, -667.0, 3008.0, 1692.0], profile_name: Some("Apple_Display".into()), icc: p3() },
        ]
    }

    /// One frame of the main window at `pos` (points; UI zoom 1) with `focused`.
    fn frame(app: &mut PhotocraftApp, ctx: &egui::Context, t: f64, pos: [f32; 2], monitor: [f32; 2], focused: bool) {
        let mut input = egui::RawInput { time: Some(t), focused, ..Default::default() };
        let vp = input.viewports.entry(egui::ViewportId::ROOT).or_default();
        vp.native_pixels_per_point = Some(1.0);
        vp.outer_rect = Some(egui::Rect::from_min_size(egui::pos2(pos[0], pos[1]), egui::vec2(800.0, 600.0)));
        vp.monitor_size = Some(egui::vec2(monitor[0], monitor[1]));
        vp.focused = Some(focused);
        let mut out = ctx.run_ui(input, |ui| poll(app, ui.ctx()));
        out.textures_delta.clear();
    }

    /// A reader that counts its readings and returns `displays` each time.
    fn reader(app: &mut PhotocraftApp, displays: Vec<Display>) -> Arc<std::sync::atomic::AtomicUsize> {
        let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = n.clone();
        app.services.read_displays = Some(Arc::new(move || {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (tx, rx) = std::sync::mpsc::channel();
            let _ = tx.send(Ok(displays.clone()));
            Some(rx)
        }));
        n
    }

    #[test]
    fn a_late_reading_is_applied_and_windows_follow_their_display() {
        let mut app = app();
        let ctx = egui::Context::default();
        let (tx, rx) = std::sync::mpsc::channel();
        pending(&mut app, rx);
        frame(&mut app, &ctx, 0.0, [100.0, 100.0], [1728.0, 1117.0], true);
        assert!(app.monitors.pending.is_some(), "still waiting");
        assert!(summary_lines(&app)[0].contains("hasn't been read yet"), "{:?}", summary_lines(&app));
        tx.send(Ok(displays(None))).unwrap();
        frame(&mut app, &ctx, 0.1, [100.0, 100.0], [1728.0, 1117.0], true);
        assert!(app.monitors.pending.is_none());
        // On the built-in display (no profile here): sRGB, reported once.
        assert_eq!(app.session.color.main_display, Some(1));
        assert_eq!(app.ui.notices.len(), 1);
        assert!(app.ui.notices[0].lines[0].contains("Built-in Retina Display has no ICC profile"));
        // Moved to the external display: its profile, without a restart.
        frame(&mut app, &ctx, 0.2, [2000.0, -500.0], [3008.0, 1692.0], true);
        assert_eq!(app.session.color.main_display, Some(4));
        assert_eq!(app.session.color.monitor().description, "Display P3");
        let lines = summary_lines(&app);
        assert_eq!(lines[1], "Monitor profile on ROG PG32UQX (main window): Apple_Display (auto)", "{lines:?}");
        assert!(lines[0].starts_with("Monitor profile on Built-in Retina Display: sRGB"), "{lines:?}");
    }

    #[test]
    fn displays_are_read_again_when_they_may_have_changed() {
        let mut app = app();
        let ctx = egui::Context::default();
        app.session.color.set_displays(Ok(displays(p3())));
        let reads = reader(&mut app, displays(p3()));
        let count = || reads.load(std::sync::atomic::Ordering::SeqCst);
        frame(&mut app, &ctx, 10.0, [100.0, 100.0], [1728.0, 1117.0], true);
        assert_eq!(count(), 0, "nothing changed");
        // Back to the front after another app had it (e.g. System Settings).
        frame(&mut app, &ctx, 11.0, [100.0, 100.0], [1728.0, 1117.0], false);
        frame(&mut app, &ctx, 12.0, [100.0, 100.0], [1728.0, 1117.0], true);
        assert_eq!(count(), 1);
        // The display reports another size (resolution changed): wanted, but not within 5 s.
        frame(&mut app, &ctx, 13.0, [100.0, 100.0], [1512.0, 982.0], true);
        frame(&mut app, &ctx, 14.0, [100.0, 100.0], [1512.0, 982.0], true);
        assert_eq!(count(), 1);
        frame(&mut app, &ctx, 17.5, [100.0, 100.0], [1512.0, 982.0], true);
        assert_eq!(count(), 2);
        // A window on no known display (one was connected).
        app.monitors.started = None;
        frame(&mut app, &ctx, 30.0, [9000.0, 100.0], [1920.0, 1080.0], true);
        frame(&mut app, &ctx, 30.1, [9000.0, 100.0], [1920.0, 1080.0], true);
        assert!(count() >= 3);
    }

    #[test]
    fn a_reader_that_dies_is_a_fallback() {
        let mut app = app();
        let (tx, rx) = std::sync::mpsc::channel::<Detection>();
        pending(&mut app, rx);
        drop(tx);
        poll(&mut app, &egui::Context::default());
        assert!(app.monitors.pending.is_none());
        assert_eq!(app.ui.notices.len(), 1);
        assert!(app.ui.notices[0].lines[0].contains("stopped without an answer"));
    }

    #[test]
    fn manual_profiles_are_quiet_and_a_failed_reread_keeps_the_displays() {
        let mut app = app();
        apply(&mut app, Ok(displays(p3())));
        assert!(app.ui.notices.is_empty());
        apply(&mut app, Err("osascript failed".into()));
        assert_eq!(app.session.color.displays.len(), 2);
        assert!(app.ui.notices.is_empty());
        app.run("edit.colorSettings", serde_json::json!({"monitorProfile": "srgb"})).unwrap();
        apply(&mut app, Ok(displays(Some(Arc::new(vec![0u8; 200])))));
        assert!(app.ui.notices.is_empty());
        assert!(summary_lines(&app).iter().all(|l| l.contains("(manual)")), "{:?}", summary_lines(&app));
    }
}
