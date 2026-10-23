//! Optional desktop status surface, independent of the `WebView` and server history.
use crate::activity::{ActivityTracker, Phase};
use std::time::{Duration, Instant};
use tauri::{
    Manager as _,
    image::Image,
    menu::{Menu, MenuItem},
    tray::{TrayIcon, TrayIconBuilder},
};
use tokio_util::sync::CancellationToken;

pub(crate) struct Owner {
    stop: CancellationToken,
    task: tauri::async_runtime::JoinHandle<()>,
    _tray: TrayIcon,
}

impl Owner {
    // Called by Tauri setup on its main thread. Failure never prevents client startup.
    pub(crate) fn install(
        app: &tauri::AppHandle,
        activity: ActivityTracker,
    ) -> tauri::Result<Self> {
        let status = MenuItem::with_id(app, "waltr-status", "Idle", false, None::<&str>)?;
        let open = MenuItem::with_id(app, "waltr-open", "Open Waltr", true, None::<&str>)?;
        let quit = MenuItem::with_id(app, "waltr-quit", "Quit", true, None::<&str>)?;
        let menu = Menu::with_items(app, &[&status, &open, &quit])?;
        let tray = TrayIconBuilder::with_id("waltr-activity")
            .icon(icon(Phase::Idle, 0))
            .icon_as_template(true)
            .menu(&menu)
            .on_menu_event(|app, event| match event.id.as_ref() {
                "waltr-open" => {
                    if let Some(window) = app.get_webview_window("main") {
                        let _result = window.unminimize();
                        let _result = window.show();
                        let _result = window.set_focus();
                    }
                }
                "waltr-quit" => app.exit(0),
                _ => {}
            })
            .build(app)?;
        let stop = CancellationToken::new();
        let cancelled = stop.clone();
        let handle = app.clone();
        let rendered_tray = tray.clone();
        let task = tauri::async_runtime::spawn(async move {
            let start = Instant::now();
            let mut previous = (Phase::Idle, 0);
            loop {
                tokio::select! {
                    () = cancelled.cancelled() => break,
                    () = tokio::time::sleep(Duration::from_millis(250)) => {}
                }
                let now = Instant::now();
                let phase = activity.phase(now);
                let frame = frame(phase, now.duration_since(start));
                if previous == (phase, frame) {
                    continue;
                }
                let (sent, received) = tokio::sync::oneshot::channel();
                let tray = rendered_tray.clone();
                let status = status.clone();
                let stopped = cancelled.clone();
                if handle
                    .run_on_main_thread(move || {
                        if stopped.is_cancelled() {
                            return;
                        }
                        let result = tray.set_icon(Some(icon(phase, frame))).and_then(|()| {
                            tray.set_icon_as_template(true)?;
                            status.set_text(match phase {
                                Phase::Idle => "Idle",
                                Phase::Working => "Working",
                                Phase::Error => "Error",
                            })
                        });
                        let _result = sent.send(result.is_ok());
                    })
                    .is_err()
                {
                    break;
                }
                // At most one render is queued; a stalled UI cannot build an update backlog.
                let ok = tokio::select! {
                    () = cancelled.cancelled() => false,
                    result = received => result.unwrap_or(false),
                };
                if !ok {
                    break;
                }
                previous = (phase, frame);
            }
        });
        Ok(Self {
            stop,
            task,
            _tray: tray,
        })
    }

    pub(crate) fn shutdown(&self) {
        self.stop.cancel();
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.stop.cancel();
        self.task.abort();
    }
}

fn frame(phase: Phase, elapsed: Duration) -> u8 {
    if phase == Phase::Working {
        ((elapsed.as_millis() / 250) % 4) as u8
    } else {
        0
    }
}

fn icon(phase: Phase, frame: u8) -> Image<'static> {
    let mut rgba = vec![0; 24 * 24 * 4];
    for y in 0_u8..24 {
        for x in 0_u8..24 {
            let dx = i32::from(x) - 12;
            let dy = i32::from(y) - 12;
            let radius = dx * dx + dy * dy;
            let ink = match phase {
                Phase::Idle => (49..=81).contains(&radius),
                Phase::Working => {
                    let quadrant = u8::from(dx >= 0) + 2 * u8::from(dy >= 0);
                    (36..=100).contains(&radius) && quadrant != frame
                }
                Phase::Error => {
                    (10..=13).contains(&x) && ((4..=14).contains(&y) || (17..=19).contains(&y))
                }
            };
            if ink {
                let offset = (usize::from(y) * 24 + usize::from(x)) * 4;
                // macOS uses the alpha template; Linux gets a contrasting opaque glyph.
                let shade = if cfg!(target_os = "macos") { 0 } else { 235 };
                rgba[offset..offset + 4].copy_from_slice(&[shade, shade, shade, 255]);
            } else if cfg!(target_os = "linux") && radius <= 121 {
                let offset = (usize::from(y) * 24 + usize::from(x)) * 4;
                rgba[offset..offset + 4].copy_from_slice(&[30, 30, 30, 255]);
            }
        }
    }
    Image::new_owned(rgba, 24, 24)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clock_frames_and_static_states_are_bounded_and_distinct() {
        assert_eq!(frame(Phase::Working, Duration::from_millis(250)), 1);
        assert_eq!(frame(Phase::Working, Duration::from_secs(1)), 0);
        assert_eq!(frame(Phase::Idle, Duration::from_secs(1)), 0);
        let idle = icon(Phase::Idle, 0);
        let active = icon(Phase::Working, 0);
        assert_ne!(idle.rgba(), active.rgba());
        assert_ne!(active.rgba(), icon(Phase::Working, 1).rgba());
        assert_ne!(idle.rgba(), icon(Phase::Error, 0).rgba());
        assert_eq!(idle.rgba().len(), 24 * 24 * 4);
    }
}
