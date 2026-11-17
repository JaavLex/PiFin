//! Frame timing: render time per frame, reported every 5 s when PIFIN_FRAMELOG is set.

use crate::App;
use slint::ComponentHandle;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Logs the first frame and, with PIFIN_FRAMELOG, frame times. Keep the timer alive.
pub(crate) fn frame_log(app: &App, t0: Instant) -> slint::Timer {
    let frames: Rc<RefCell<(Option<Instant>, Vec<f64>)>> = Rc::new(RefCell::new((None, vec![])));
    {
        let frames = frames.clone();
        let mut first = true;
        let weak = app.as_weak();
        app.window()
            .set_rendering_notifier(move |state, _| {
                let mut f = frames.borrow_mut();
                match state {
                    slint::RenderingState::BeforeRendering => f.0 = Some(Instant::now()),
                    slint::RenderingState::AfterRendering => {
                        if let Some(st) = f.0.take() {
                            f.1.push(st.elapsed().as_secs_f64() * 1000.0);
                        }
                        if first {
                            first = false;
                            let size = weak
                                .upgrade()
                                .map(|a| a.window().size())
                                .unwrap_or_default();
                            eprintln!(
                                "FIRST_FRAME_MS={} window={}x{}",
                                t0.elapsed().as_millis(),
                                size.width,
                                size.height
                            );
                        }
                    }
                    _ => {}
                }
            })
            .ok();
    }
    let frame_timer = slint::Timer::default();
    if std::env::var_os("PIFIN_FRAMELOG").is_some() {
        let frames = frames.clone();
        frame_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_secs(5),
            move || {
                let mut v = std::mem::take(&mut frames.borrow_mut().1);
                if v.is_empty() {
                    return;
                }
                v.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let p = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
                eprintln!(
                    "FRAMES n={} p50={:.1}ms p95={:.1}ms max={:.1}ms",
                    v.len(),
                    p(0.5),
                    p(0.95),
                    v[v.len() - 1]
                );
            },
        );
    }
    frame_timer
}
