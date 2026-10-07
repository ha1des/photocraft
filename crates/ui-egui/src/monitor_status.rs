//! The monitor profile in the shell (#569): applies the platform's display profile reading
//! whenever it arrives, says when the canvas falls back to sRGB instead of the requested
//! profile, and gives Help › System Info its line.

use photocraft_engine::display_color::{DetectedMonitor, MonitorDetection};

use crate::PhotocraftApp;

/// A platform reading: the display's profile, or why there is none.
pub type Detection = Result<DetectedMonitor, String>;

/// Notice title when the display's profile can't be used.
pub const FALLBACK_TITLE: &str = "Display profile not used";

/// Per-frame: apply a reading that arrived after startup (`Services::monitor_profile`).
pub fn poll(app: &mut PhotocraftApp, ctx: &egui::Context) {
    let Some(rx) = &app.services.monitor_profile else { return };
    let r = match rx.try_recv() {
        Ok(r) => r,
        Err(std::sync::mpsc::TryRecvError::Empty) => {
            // No input may come for a while: keep a frame scheduled to pick it up.
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
            return;
        }
        Err(std::sync::mpsc::TryRecvError::Disconnected) => Err("the display profile reader stopped without an answer".into()),
    };
    app.services.monitor_profile = None;
    apply(app, r);
    ctx.request_repaint();
}

/// Record a reading. When it leaves the canvas on sRGB instead of the display's profile, say so:
/// an unusable profile must not look like working colour management.
pub fn apply(app: &mut PhotocraftApp, r: Detection) {
    app.session.color.set_detected_monitor(r);
    let st = app.session.color.monitor_status();
    if st.source != "fallback" || matches!(st.detection, MonitorDetection::Unsupported | MonitorDetection::Pending) {
        return;
    }
    let mut lines: Vec<String> = st.reason.into_iter().collect();
    lines.push("The canvas is shown as sRGB. Choose a monitor profile in Edit › Color Settings.".into());
    crate::notices::post(app, FALLBACK_TITLE, lines, true);
}

/// Help › System Info's line.
pub fn summary(app: &PhotocraftApp) -> String {
    format!("Monitor profile: {}", app.session.color.monitor_status().summary())
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_engine::Session;

    fn app() -> PhotocraftApp {
        PhotocraftApp::new(Session::new(), Default::default())
    }

    fn display(icc: Vec<u8>) -> Detection {
        Ok(DetectedMonitor { display: "ROG PG32UQX".into(), icc })
    }

    fn p3() -> Vec<u8> {
        photocraft_engine::color_cmds::resolve_profile("display-p3", None, None).unwrap().to_bytes().to_vec()
    }

    #[test]
    fn a_late_reading_is_applied() {
        let mut app = app();
        let ctx = egui::Context::default();
        let (tx, rx) = std::sync::mpsc::channel();
        app.session.color.monitor_detection = MonitorDetection::Pending;
        app.services.monitor_profile = Some(rx);
        poll(&mut app, &ctx);
        assert!(app.services.monitor_profile.is_some(), "still waiting");
        assert!(summary(&app).contains("hasn't been read yet"), "{}", summary(&app));
        tx.send(display(p3())).unwrap();
        poll(&mut app, &ctx);
        assert!(app.services.monitor_profile.is_none());
        assert_eq!(app.session.color.monitor().description, "Display P3");
        assert_eq!(summary(&app), "Monitor profile: Display P3 (auto for ROG PG32UQX)");
        assert!(app.ui.notices.is_empty());
    }

    #[test]
    fn a_reader_that_dies_is_a_fallback() {
        let mut app = app();
        let (tx, rx) = std::sync::mpsc::channel::<Detection>();
        app.services.monitor_profile = Some(rx);
        drop(tx);
        poll(&mut app, &egui::Context::default());
        assert!(app.services.monitor_profile.is_none());
        assert_eq!(app.ui.notices.len(), 1);
        assert!(app.ui.notices[0].lines[0].contains("stopped without an answer"));
    }

    #[test]
    fn unusable_profiles_are_reported_once_and_manual_choices_are_quiet() {
        let mut app = app();
        apply(&mut app, display(vec![0u8; 200]));
        assert_eq!(app.ui.notices.len(), 1);
        let n = &app.ui.notices[0];
        assert_eq!(n.title, FALLBACK_TITLE);
        assert!(n.error && n.lines[0].contains("can't be read"), "{:?}", n.lines);
        assert!(summary(&app).contains("fallback"));
        // A manual monitor profile doesn't depend on the reading.
        app.run("edit.colorSettings", serde_json::json!({"monitorProfile": "srgb"})).unwrap();
        apply(&mut app, Err("osascript failed".into()));
        assert_eq!(app.ui.notices.len(), 1);
        assert!(summary(&app).contains("(manual)"), "{}", summary(&app));
    }
}
