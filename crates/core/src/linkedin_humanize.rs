//! Human-like pacing and motion for the LinkedIn browser (mirrors
//! `bin/linkedin_humanize.py`). [`Humanizer`] is pure: its RNG, sleep and clock
//! are injectable so tests are deterministic. [`HumanPage`] is the only code
//! that drives a browser page (through the [`PageDriver`] seam), so every
//! navigation, scroll and cursor movement is paced.

use crate::loop_plugins::linkedin_autopilot::{BrowserError, Human};
use std::sync::{Arc, Mutex};

pub const MIN_GAP_S: f64 = 2.5; // hard floor between two page loads (jitter on top)
pub const DWELL_MIN_S: f64 = 5.0;
pub const DWELL_MAX_S: f64 = 60.0;
pub const READ_CHARS_PER_S: f64 = 22.0;
pub const START_DELAY_MAX_S: f64 = 1800.0;
pub const RENDER_TIMEOUT_S: f64 = 600.0; // 10 minutes, then give up on the page
pub const RENDER_POLL_S: f64 = 2.0;
pub const RENDER_MIN_CHARS: i64 = 300; // less visible text is still the loading shell
pub const SKIP_DAY_P: f64 = 0.1;
pub const WANDER_MIN: usize = 1;
pub const WANDER_MAX: usize = 3;

/// `'9-21'` -> `(9, 21)`; anything malformed or out of range -> `default`.
pub fn parse_active_hours(text: Option<&str>, default: (i64, i64)) -> (i64, i64) {
    let Some((a, b)) = text.and_then(|t| t.split_once('-')) else {
        return default;
    };
    let num = |s: &str| {
        let t = s.trim();
        (!t.is_empty() && t.len() <= 2 && t.chars().all(|c| c.is_ascii_digit()))
            .then(|| t.parse::<i64>().ok())
            .flatten()
    };
    match (num(a), num(b)) {
        (Some(s), Some(e)) if s < e && e <= 24 => (s, e),
        _ => default,
    }
}

pub fn within_hours(hour: i64, window: (i64, i64)) -> bool {
    window.0 <= hour && hour < window.1
}

// ---- RNG ------------------------------------------------------------------------------

/// splitmix64: tiny, seedable, good enough for pacing jitter.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn seeded(seed: u64) -> Self {
        Rng(seed)
    }

    pub fn from_clock() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |d| d.as_nanos() as u64);
        Rng(nanos ^ (u64::from(std::process::id()) << 32))
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    pub fn random(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn uniform(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.random()
    }

    /// Inclusive on both ends, like `random.randint`.
    pub fn randint(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.random() * (hi - lo + 1) as f64) as i64
    }

    /// Standard normal (Box-Muller).
    pub fn gauss(&mut self) -> f64 {
        let u1 = (1.0 - self.random()).max(f64::MIN_POSITIVE);
        let u2 = self.random();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }

    pub fn lognormvariate(&mut self, mu: f64, sigma: f64) -> f64 {
        (mu + sigma * self.gauss()).exp()
    }

    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = (self.random() * (i + 1) as f64) as usize;
            items.swap(i, j.min(i));
        }
    }
}

// ---- the humanizer ----------------------------------------------------------------------

pub type SleepFn = Arc<dyn Fn(f64) + Send + Sync>;
pub type ClockFn = Arc<dyn Fn() -> f64 + Send + Sync>;

struct State {
    rng: Rng,
    last_load: Option<f64>,
}

pub struct Humanizer {
    state: Mutex<State>,
    sleep_fn: SleepFn,
    clock_fn: ClockFn,
}

impl Default for Humanizer {
    fn default() -> Self {
        Self::new(Rng::from_clock(), None, None)
    }
}

impl Humanizer {
    /// `sleep` defaults to a real sleep, `clock` to a monotonic clock in seconds.
    pub fn new(rng: Rng, sleep: Option<SleepFn>, clock: Option<ClockFn>) -> Self {
        let sleep_fn: SleepFn = sleep.unwrap_or_else(|| {
            Arc::new(|s: f64| std::thread::sleep(std::time::Duration::from_secs_f64(s.max(0.0))))
        });
        let clock_fn: ClockFn = clock.unwrap_or_else(|| {
            let start = std::time::Instant::now();
            Arc::new(move || start.elapsed().as_secs_f64())
        });
        Self {
            state: Mutex::new(State {
                rng,
                last_load: None,
            }),
            sleep_fn,
            clock_fn,
        }
    }

    fn rng<R>(&self, f: impl FnOnce(&mut Rng) -> R) -> R {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut s.rng)
    }

    pub fn clock(&self) -> f64 {
        (self.clock_fn)()
    }

    pub fn sleep(&self, seconds: f64) {
        (self.sleep_fn)(seconds.max(0.0));
    }

    pub fn uniform(&self, lo: f64, hi: f64) -> f64 {
        self.rng(|r| r.uniform(lo, hi))
    }

    pub fn dwell_seconds(&self, text_len: usize) -> f64 {
        self.rng(|r| {
            let base = r.lognormvariate(12.0f64.ln(), 0.5);
            let read = text_len as f64 / READ_CHARS_PER_S * r.uniform(0.3, 0.7);
            (base + read).max(DWELL_MIN_S).min(DWELL_MAX_S)
        })
    }

    pub fn pause_seconds(&self, lo: f64, hi: f64) -> f64 {
        self.uniform(lo, hi)
    }

    /// Seconds to wait so two page loads are never closer than [`MIN_GAP_S`]
    /// (plus jitter). Records the load as happening after the wait.
    pub fn gap_before_load(&self) -> f64 {
        let now = self.clock();
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut wait = 0.0;
        if let Some(last) = s.last_load {
            let jitter = s.rng.uniform(0.0, 3.0);
            wait = (MIN_GAP_S + jitter - (now - last)).max(0.0);
        }
        s.last_load = Some(now + wait);
        wait
    }

    /// Uneven `(delta_px, pause_s)` wheel steps reaching at least `total_px`,
    /// with the occasional short scroll back up.
    pub fn scroll_plan(&self, total_px: i64) -> Vec<(i64, f64)> {
        self.rng(|r| {
            let (mut steps, mut done) = (Vec::new(), 0i64);
            while done < total_px {
                let delta = (r.uniform(180.0, 520.0) as i64).min(total_px - done).max(1);
                steps.push((delta, r.uniform(0.3, 2.2)));
                done += delta;
                if done > 600 && r.random() < 0.12 {
                    let back = -(r.uniform(80.0, 240.0) as i64);
                    steps.push((back, r.uniform(0.4, 1.5)));
                    done += back;
                }
            }
            steps
        })
    }

    /// Points along a curved (quadratic Bezier) path, sometimes overshooting the
    /// target slightly before settling on it. The last point is `end`.
    pub fn mouse_path(
        &self,
        start: (f64, f64),
        end: (f64, f64),
        steps: Option<usize>,
    ) -> Vec<(f64, f64)> {
        self.rng(|r| {
            let ((x0, y0), (x1, y1)) = (start, end);
            let n = steps.unwrap_or_else(|| r.randint(14, 30) as usize).max(1);
            let cx = (x0 + x1) / 2.0 + r.uniform(-80.0, 80.0);
            let cy = (y0 + y1) / 2.0 + r.uniform(-80.0, 80.0);
            let round1 = |v: f64| (v * 10.0).round() / 10.0;
            let mut points: Vec<(f64, f64)> = (1..=n)
                .map(|i| {
                    let t = i as f64 / n as f64;
                    let u = 1.0 - t;
                    (
                        round1(u * u * x0 + 2.0 * u * t * cx + t * t * x1),
                        round1(u * u * y0 + 2.0 * u * t * cy + t * t * y1),
                    )
                })
                .collect();
            *points.last_mut().expect("n >= 1") = end;
            if r.random() < 0.4 {
                let sx = if r.random() < 0.5 { -1.0 } else { 1.0 };
                let sy = if r.random() < 0.5 { -1.0 } else { 1.0 };
                let o = (
                    round1(x1 + sx * r.uniform(4.0, 14.0)),
                    round1(y1 + sy * r.uniform(4.0, 14.0)),
                );
                let at = points.len() - 1;
                points.insert(at, o);
            }
            points
        })
    }

    /// One to three of `candidates`, in a random order.
    pub fn wander_pages<'a>(&self, candidates: &[&'a str]) -> Vec<&'a str> {
        self.rng(|r| {
            let mut pool: Vec<&str> = candidates.to_vec();
            r.shuffle(&mut pool);
            let n = (r.randint(WANDER_MIN as i64, WANDER_MAX as i64) as usize).min(pool.len());
            pool.truncate(n);
            pool
        })
    }
}

impl Human for Humanizer {
    fn sleep(&self, seconds: f64) {
        Humanizer::sleep(self, seconds);
    }
    fn start_delay_seconds(&self) -> f64 {
        self.uniform(0.0, START_DELAY_MAX_S)
    }
    fn skip_today(&self) -> bool {
        self.rng(|r| r.random() < SKIP_DAY_P)
    }
    fn shuffle(&self, items: &mut Vec<&'static str>) {
        self.rng(|r| r.shuffle(items));
    }
}

// ---- the page seam -----------------------------------------------------------------------

/// What to click: a CSS selector, or the first element among `tags` whose text
/// contains `text` (case-insensitive).
#[derive(Debug, Clone, PartialEq)]
pub enum Selector {
    Css(String),
    Text { tags: Vec<String>, text: String },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BBox {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// A browser page: raw, unpaced primitives. Only [`HumanPage`] calls these.
pub trait PageDriver {
    /// Navigate and return at DOMContentLoaded; [`BrowserError::PageTimeout`]
    /// when it takes longer than `timeout_s`.
    fn goto(&mut self, url: &str, timeout_s: f64) -> Result<(), BrowserError>;
    fn url(&mut self) -> Result<String, BrowserError>;
    fn content(&mut self) -> Result<String, BrowserError>;
    /// Evaluate a JS function expression (`() => ...`) and return its JSON value.
    fn evaluate(&mut self, js: &str) -> Result<serde_json::Value, BrowserError>;
    /// The box of the first visible match, waiting up to `timeout_s`; `None` if absent.
    fn visible_box(
        &mut self,
        selector: &Selector,
        timeout_s: f64,
    ) -> Result<Option<BBox>, BrowserError>;
    fn mouse_move(&mut self, x: f64, y: f64) -> Result<(), BrowserError>;
    fn mouse_wheel(&mut self, dy: f64) -> Result<(), BrowserError>;
    fn mouse_click(&mut self, x: f64, y: f64) -> Result<(), BrowserError>;
    fn type_char(&mut self, ch: char) -> Result<(), BrowserError>;
    fn press_enter(&mut self) -> Result<(), BrowserError>;
}

/// Wraps a page: navigation, scrolling and cursor movement all happen here and
/// nowhere else.
pub struct HumanPage<P: PageDriver> {
    pub page: P,
    pub human: Arc<Humanizer>,
    pos: (f64, f64),
}

impl<P: PageDriver> HumanPage<P> {
    pub fn new(page: P, human: Arc<Humanizer>) -> Self {
        Self {
            page,
            human,
            pos: (400.0, 300.0),
        }
    }

    pub fn goto(&mut self, url: &str) -> Result<(), BrowserError> {
        self.human.sleep(self.human.gap_before_load());
        self.page.goto(url, RENDER_TIMEOUT_S)?;
        self.idle_mouse()
    }

    pub fn url(&mut self) -> Result<String, BrowserError> {
        self.page.url()
    }

    pub fn content(&mut self) -> Result<String, BrowserError> {
        self.page.content()
    }

    pub fn evaluate(&mut self, js: &str) -> Result<serde_json::Value, BrowserError> {
        self.page.evaluate(js)
    }

    /// LinkedIn renders client-side, so a loaded page is not a rendered page.
    /// Poll the visible text length until it is long enough and has stopped
    /// changing for two polls in a row; `PageTimeout` after `timeout` seconds.
    pub fn wait_rendered(
        &mut self,
        timeout: f64,
        poll: f64,
        min_chars: i64,
    ) -> Result<(), BrowserError> {
        let deadline = self.human.clock() + timeout;
        let (mut last, mut stable) = (-1i64, 0);
        loop {
            let size = self
                .page
                .evaluate("() => (document.body && document.body.innerText || '').length")
                .ok()
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            stable = if size == last && size >= min_chars {
                stable + 1
            } else {
                0
            };
            last = size;
            if stable >= 2 {
                return Ok(());
            }
            if self.human.clock() >= deadline {
                return Err(BrowserError::PageTimeout);
            }
            self.human.sleep(poll);
        }
    }

    pub fn wait_rendered_default(&mut self) -> Result<(), BrowserError> {
        self.wait_rendered(RENDER_TIMEOUT_S, RENDER_POLL_S, RENDER_MIN_CHARS)
    }

    fn move_cursor(&mut self, end: (f64, f64)) -> Result<(), BrowserError> {
        for (x, y) in self.human.mouse_path(self.pos, end, None) {
            self.page.mouse_move(x, y)?;
            self.human.sleep(self.human.uniform(0.005, 0.03));
        }
        self.pos = end;
        Ok(())
    }

    /// Click the first visible match like a person. `false` when nothing matches.
    pub fn click(&mut self, selector: &Selector) -> Result<bool, BrowserError> {
        let Some(b) = self.page.visible_box(selector, 8.0)? else {
            return Ok(false);
        };
        let end = (
            b.x + b.width * self.human.uniform(0.3, 0.7),
            b.y + b.height * self.human.uniform(0.3, 0.7),
        );
        self.move_cursor(end)?;
        self.human.sleep(self.human.pause_seconds(0.4, 2.5));
        self.page.mouse_click(self.pos.0, self.pos.1)?;
        Ok(true)
    }

    /// Click a field and type with uneven key delays, then Enter. `false` if the field is missing.
    pub fn type_into(&mut self, selector: &Selector, text: &str) -> Result<bool, BrowserError> {
        if !self.click(selector)? {
            return Ok(false);
        }
        for ch in text.chars() {
            self.page.type_char(ch)?;
            self.human.sleep(self.human.uniform(0.05, 0.22));
        }
        self.human.sleep(self.human.pause_seconds(0.4, 2.5));
        self.page.press_enter()?;
        Ok(true)
    }

    pub fn idle_mouse(&mut self) -> Result<(), BrowserError> {
        let end = (
            self.human.uniform(200.0, 1200.0),
            self.human.uniform(150.0, 700.0),
        );
        self.move_cursor(end)
    }

    pub fn scroll(&mut self, total_px: i64) -> Result<(), BrowserError> {
        for (delta, pause) in self.human.scroll_plan(total_px) {
            self.page.mouse_wheel(delta as f64)?;
            self.human.sleep(pause);
        }
        Ok(())
    }

    pub fn dwell(&mut self, text_len: usize) {
        self.human.sleep(self.human.dwell_seconds(text_len));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::sync::Mutex as StdMutex;

    fn human(seed: u64, now: f64) -> (Arc<Humanizer>, Arc<StdMutex<Vec<f64>>>) {
        let slept = Arc::new(StdMutex::new(Vec::new()));
        let s = slept.clone();
        let h = Humanizer::new(
            Rng::seeded(seed),
            Some(Arc::new(move |x| s.lock().unwrap().push(x))),
            Some(Arc::new(move || now)),
        );
        (Arc::new(h), slept)
    }

    #[test]
    fn dwell_is_bounded_and_grows_with_text() {
        let (h, _) = human(0, 100.0);
        let short: Vec<f64> = (0..200).map(|_| h.dwell_seconds(0)).collect();
        let long: Vec<f64> = (0..200).map(|_| h.dwell_seconds(3000)).collect();
        assert!(short
            .iter()
            .chain(&long)
            .all(|d| (DWELL_MIN_S..=DWELL_MAX_S).contains(d)));
        assert!(long.iter().sum::<f64>() > short.iter().sum::<f64>());
    }

    #[test]
    fn dwell_varies() {
        let (h, _) = human(0, 100.0);
        let set: std::collections::BTreeSet<i64> = (0..20)
            .map(|_| (h.dwell_seconds(500) * 100.0).round() as i64)
            .collect();
        assert!(set.len() > 10);
    }

    #[test]
    fn two_page_loads_are_never_closer_than_min_gap() {
        let (h, _) = human(0, 100.0); // frozen clock: the second load comes "immediately"
        assert_eq!(h.gap_before_load(), 0.0);
        assert!(h.gap_before_load() >= MIN_GAP_S);
    }

    #[test]
    fn sleep_never_negative() {
        let (h, slept) = human(0, 0.0);
        h.sleep(-3.0);
        assert_eq!(*slept.lock().unwrap(), vec![0.0]);
    }

    #[test]
    fn scroll_plan_reaches_target_unevenly_and_sometimes_goes_back() {
        let (h, _) = human(3, 0.0);
        let plans: Vec<_> = (0..40).map(|_| h.scroll_plan(4000)).collect();
        for plan in &plans {
            assert!(plan.iter().map(|(d, _)| d).sum::<i64>() >= 4000);
            assert!(plan.iter().all(|(_, p)| (0.3..=2.2).contains(p)));
        }
        let distinct: std::collections::BTreeSet<Vec<i64>> = plans
            .iter()
            .map(|p| p.iter().map(|(d, _)| *d).collect())
            .collect();
        assert!(distinct.len() > 20);
        assert!(plans.iter().flatten().any(|(d, _)| *d < 0));
    }

    #[test]
    fn scroll_plan_terminates_for_small_and_zero() {
        let (h, _) = human(0, 0.0);
        assert!(h.scroll_plan(0).is_empty());
        assert!(h.scroll_plan(50).iter().map(|(d, _)| d).sum::<i64>() >= 50);
    }

    #[test]
    fn mouse_path_is_curved_and_ends_on_target() {
        let (h, _) = human(5, 0.0);
        let path = h.mouse_path((0.0, 0.0), (600.0, 0.0), None);
        assert!(path.len() >= 14 && *path.last().unwrap() == (600.0, 0.0));
        assert!(path.iter().any(|(_, y)| y.abs() > 1.0));
    }

    #[test]
    fn wander_pages_is_a_small_random_subset() {
        let (h, _) = human(1, 0.0);
        let pages = ["a", "b", "c", "d", "e"];
        let picks: Vec<_> = (0..30).map(|_| h.wander_pages(&pages)).collect();
        for p in &picks {
            let uniq: std::collections::BTreeSet<_> = p.iter().collect();
            assert!((1..=3).contains(&p.len()) && uniq.len() == p.len());
            assert!(p.iter().all(|x| pages.contains(x)));
        }
        let distinct: std::collections::BTreeSet<_> = picks.iter().collect();
        assert!(distinct.len() > 5);
        assert!(h.wander_pages(&[]).is_empty());
    }

    #[test]
    fn skip_today_is_occasional() {
        let (h, _) = human(2, 0.0);
        let skips = (0..2000).filter(|_| Human::skip_today(&*h)).count();
        assert!((80..330).contains(&skips), "{skips}");
    }

    #[test]
    fn start_delay_within_bounds() {
        let (h, _) = human(0, 0.0);
        assert!((0..100).all(|_| (0.0..=START_DELAY_MAX_S).contains(&h.start_delay_seconds())));
    }

    #[test]
    fn parse_active_hours_cases() {
        let d = (9, 21);
        assert_eq!(parse_active_hours(Some("8-18"), d), (8, 18));
        assert_eq!(parse_active_hours(Some(" 9 - 21 "), d), (9, 21));
        for bad in ["", "x", "18-9", "9-25", "-3-5", "9"] {
            assert_eq!(parse_active_hours(Some(bad), d), d, "{bad}");
        }
        assert_eq!(parse_active_hours(None, d), d);
        assert_eq!(parse_active_hours(Some("bad"), (1, 2)), (1, 2));
    }

    #[test]
    fn within_hours_is_half_open() {
        assert!(within_hours(9, (9, 21)) && within_hours(20, (9, 21)));
        assert!(!within_hours(21, (9, 21)) && !within_hours(8, (9, 21)));
    }

    /// Records every primitive in order.
    #[derive(Default)]
    struct FakePage {
        events: Arc<StdMutex<Vec<String>>>,
        lengths: Vec<i64>,
        wheels: Vec<f64>,
        goto_timeout: bool,
    }

    impl PageDriver for FakePage {
        fn goto(&mut self, url: &str, _t: f64) -> Result<(), BrowserError> {
            if self.goto_timeout {
                return Err(BrowserError::PageTimeout);
            }
            self.events.lock().unwrap().push(format!("goto {url}"));
            Ok(())
        }
        fn url(&mut self) -> Result<String, BrowserError> {
            Ok(String::new())
        }
        fn content(&mut self) -> Result<String, BrowserError> {
            Ok(String::new())
        }
        fn evaluate(&mut self, _js: &str) -> Result<Value, BrowserError> {
            let n = if self.lengths.len() > 1 {
                self.lengths.remove(0)
            } else {
                self.lengths.first().copied().unwrap_or(0)
            };
            Ok(json!(n))
        }
        fn visible_box(&mut self, _s: &Selector, _t: f64) -> Result<Option<BBox>, BrowserError> {
            Ok(None)
        }
        fn mouse_move(&mut self, _x: f64, _y: f64) -> Result<(), BrowserError> {
            self.events.lock().unwrap().push("move".into());
            Ok(())
        }
        fn mouse_wheel(&mut self, dy: f64) -> Result<(), BrowserError> {
            self.wheels.push(dy);
            Ok(())
        }
        fn mouse_click(&mut self, _x: f64, _y: f64) -> Result<(), BrowserError> {
            Ok(())
        }
        fn type_char(&mut self, _c: char) -> Result<(), BrowserError> {
            Ok(())
        }
        fn press_enter(&mut self) -> Result<(), BrowserError> {
            Ok(())
        }
    }

    /// A human whose sleeps land in the same event list as the page's.
    fn logged(events: &Arc<StdMutex<Vec<String>>>) -> Arc<Humanizer> {
        let e = events.clone();
        Arc::new(Humanizer::new(
            Rng::seeded(0),
            Some(Arc::new(move |s| e.lock().unwrap().push(format!("sleep {s}")))),
            Some(Arc::new(|| 50.0)),
        ))
    }

    #[test]
    fn goto_waits_for_gap_navigates_then_moves_cursor() {
        let events = Arc::new(StdMutex::new(Vec::new()));
        let page = FakePage {
            events: events.clone(),
            ..Default::default()
        };
        let mut hp = HumanPage::new(page, logged(&events));
        hp.goto("feed").unwrap();
        hp.goto("jobs").unwrap();
        let ev = events.lock().unwrap().clone();
        let gotos: Vec<usize> = ev
            .iter()
            .enumerate()
            .filter(|(_, e)| e.starts_with("goto"))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(gotos.len(), 2);
        let (first, second) = (gotos[0], gotos[1]);
        let slept = |e: &String| e.strip_prefix("sleep ").and_then(|s| s.parse::<f64>().ok());
        assert!(!ev[..first]
            .iter()
            .any(|e| slept(e).is_some_and(|s| s >= MIN_GAP_S)));
        assert!(first < second - 1 && slept(&ev[second - 1]).is_some_and(|s| s >= MIN_GAP_S));
        for (g, nxt) in [(first, second), (second, ev.len())] {
            assert_eq!(ev[g + 1], "move");
            assert!(ev[g + 1..nxt].iter().any(|e| e == "move"));
        }
    }

    #[test]
    fn scroll_uses_wheel_steps_and_pauses() {
        let (h, slept) = human(4, 0.0);
        let mut hp = HumanPage::new(FakePage::default(), h);
        hp.scroll(2000).unwrap();
        assert!(hp.page.wheels.iter().sum::<f64>() >= 2000.0);
        assert_eq!(slept.lock().unwrap().len(), hp.page.wheels.len());
    }

    #[test]
    fn dwell_sleeps_once() {
        let (h, slept) = human(0, 0.0);
        let mut hp = HumanPage::new(FakePage::default(), h);
        hp.dwell(800);
        let s = slept.lock().unwrap();
        assert!(s.len() == 1 && (DWELL_MIN_S..=DWELL_MAX_S).contains(&s[0]));
    }

    /// A human whose clock advances with its sleeps.
    fn ticking() -> Arc<Humanizer> {
        let t = Arc::new(StdMutex::new(0.0f64));
        let (t1, t2) = (t.clone(), t);
        Arc::new(Humanizer::new(
            Rng::seeded(0),
            Some(Arc::new(move |s| *t1.lock().unwrap() += s)),
            Some(Arc::new(move || *t2.lock().unwrap())),
        ))
    }

    #[test]
    fn wait_rendered_returns_once_the_page_text_stops_growing() {
        let page = FakePage {
            lengths: vec![0, 50, 400, 900, 2000, 2000, 2000],
            ..Default::default()
        };
        HumanPage::new(page, ticking())
            .wait_rendered_default()
            .unwrap();
    }

    #[test]
    fn wait_rendered_gives_up_after_ten_minutes_of_a_blank_page() {
        let h = ticking();
        let page = FakePage {
            lengths: vec![10],
            ..Default::default()
        };
        let err = HumanPage::new(page, h.clone())
            .wait_rendered_default()
            .unwrap_err();
        assert_eq!(err, BrowserError::PageTimeout);
        assert!((600.0..620.0).contains(&h.clock()), "{}", h.clock());
    }

    #[test]
    fn wait_rendered_keeps_waiting_while_the_page_keeps_changing() {
        let mut lengths: Vec<i64> = (0..40).map(|i| 500 + i * 100).collect();
        lengths.extend([9999; 3]);
        let page = FakePage {
            lengths,
            ..Default::default()
        };
        HumanPage::new(page, ticking())
            .wait_rendered_default()
            .unwrap();
    }

    #[test]
    fn a_goto_timeout_is_page_timeout() {
        let page = FakePage {
            goto_timeout: true,
            ..Default::default()
        };
        let err = HumanPage::new(page, ticking()).goto("x").unwrap_err();
        assert_eq!(err, BrowserError::PageTimeout);
    }
}
