//! RATATUI OPEN — a tiny 3D tennis game for the terminal.
//!
//! cargo run --release   (best in a terminal ≥ 100×30)
//!
//! ◀ ▶ / A D  move along the baseline      ▲ ▼ / W S  step in / back
//! SPACE      serve                        P pause    R rematch    Q quit

use std::f64::consts::PI;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Color::Rgb, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget};
use ratatui::{DefaultTerminal, Frame};

/// Frames a direction stays live after the last press/repeat. Fallback for
/// terminals that never send key-release events; ~0.5s bridges the OS
/// auto-repeat initial delay so holding a key still moves continuously.
const KEY_HOLD_FRAMES: u8 = 30;

// ── court & physics (metres, seconds) ────────────────────────────────────────
const DT: f64 = 1.0 / 60.0;
const COURT_LEN: f64 = 23.77;
const HALF_W: f64 = 4.115; // singles half-width
const DOUBLES_W: f64 = 4.875;
const NET_Z: f64 = COURT_LEN / 2.0;
const NET_H: f64 = 0.914;
const SVC_NEAR: f64 = NET_Z - 6.40;
const SVC_FAR: f64 = NET_Z + 6.40;
const GRAVITY: f64 = 7.6;
const GAMES_TO_WIN: u8 = 3;

mod pal {
    use ratatui::style::Color;
    use ratatui::style::Color::Rgb;
    pub const BALL: Color = Rgb(228, 255, 64);
    pub const LINE: Color = Rgb(240, 246, 255);
    pub const COURT: [Color; 2] = [Rgb(36, 88, 154), Rgb(43, 99, 170)];
    pub const APRON: [Color; 2] = [Rgb(28, 110, 88), Rgb(34, 122, 99)];
    pub const GROUND: Color = Rgb(13, 17, 26);
    pub const WALL: Color = Rgb(24, 34, 64);
    pub const YOU: Color = Rgb(255, 122, 92);
    pub const CPU: Color = Rgb(96, 200, 255);
    pub const GOOD: Color = Rgb(140, 255, 170);
    pub const BAD: Color = Rgb(255, 110, 110);
    pub const INFO: Color = Rgb(255, 230, 120);
}

#[derive(Clone, Copy)]
struct V3 {
    x: f64,
    y: f64,
    z: f64,
}
impl V3 {
    fn new(x: f64, y: f64, z: f64) -> Self {
        V3 { x, y, z }
    }
}

// ── perspective camera ───────────────────────────────────────────────────────
struct Cam {
    cx: f64,
    horizon: f64,
    f: f64,
    vs: f64,
    dist: f64,
    h: f64,
}

impl Cam {
    fn for_rect(r: Rect) -> Self {
        Cam {
            cx: r.x as f64 + r.width as f64 / 2.0,
            horizon: r.y as f64 + (r.height as f64 * 0.30).max(4.0),
            f: r.width as f64 * 0.42,   // horizontal focal
            vs: r.width as f64 * 0.215, // vertical (cells are ~2:1)
            dist: 4.6,                  // camera behind baseline
            h: 3.1,                     // camera height
        }
    }
    fn proj(&self, p: V3) -> Option<(f64, f64)> {
        let d = p.z + self.dist;
        if d < 0.7 {
            return None;
        }
        Some((
            self.cx + p.x * self.f / d,
            self.horizon + (self.h - p.y) * self.vs / d,
        ))
    }
    fn z_at_row(&self, row: f64) -> f64 {
        (self.h * self.vs) / (row - self.horizon) - self.dist
    }
    fn x_at_col(&self, col: f64, d: f64) -> f64 {
        (col - self.cx) * d / self.f
    }
}

// ── game state ───────────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Serve { server: u8, wait: f32 },
    Rally,
    Point(f32),
    Over,
}

struct Ball {
    p: V3,
    v: V3,
    live: bool,
}
struct Particle {
    p: V3,
    v: V3,
    life: f32,
}
struct Score {
    pts: [u8; 2],
    games: [u8; 2],
    server: u8,
    serves: u8,
}

struct Game {
    ball: Ball,
    px: f64,
    pz: f64,
    ax: f64,
    az: f64,
    dir: i8,
    vdir: i8,
    phase: Phase,
    score: Score,
    last_hitter: u8, // 0 = you, 1 = cpu, 2 = none
    bounces: u8,
    is_serve: bool,
    net_touched: bool,
    rally: u32,
    msg: Option<(String, Color, f32)>,
    sub: Option<(String, f32)>,
    trail: Vec<V3>,
    dust: Vec<Particle>,
    frame: u64,
    rng: u64,
    paused: bool,
    dir_ttl: u8,
    vdir_ttl: u8,
}

fn pt_str(p: u8) -> &'static str {
    match p {
        0 => "0",
        1 => "15",
        2 => "30",
        _ => "40",
    }
}

fn pt_label(me: u8, other: u8) -> String {
    if me >= 3 && other >= 3 {
        if me > other { "AD".into() } else { "40".into() }
    } else {
        pt_str(me).into()
    }
}

impl Game {
    fn new() -> Self {
        let mut g = Game {
            ball: Ball {
                p: V3::new(0.0, 1.0, 1.0),
                v: V3::new(0.0, 0.0, 0.0),
                live: false,
            },
            px: 0.0,
            pz: 0.6,
            ax: 0.0,
            az: COURT_LEN - 0.9,
            dir: 0,
            vdir: 0,
            phase: Phase::Serve {
                server: 0,
                wait: 0.9,
            },
            score: Score {
                pts: [0, 0],
                games: [0, 0],
                server: 0,
                serves: 2,
            },
            last_hitter: 2,
            bounces: 0,
            is_serve: false,
            net_touched: false,
            rally: 0,
            msg: None,
            sub: None,
            trail: Vec::new(),
            dust: Vec::new(),
            frame: 0,
            rng: 0x9E3779B97F4A7C15,
            paused: false,
            dir_ttl: 0,
            vdir_ttl: 0,
        };
        g.begin_serve();
        g
    }

    fn rnd(&mut self) -> f64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }

    fn flash(&mut self, s: &str, c: Color, t: f32) {
        self.msg = Some((s.into(), c, t));
    }

    fn begin_serve(&mut self) {
        self.phase = Phase::Serve {
            server: self.score.server,
            wait: 0.9,
        };
        self.ball.live = false;
        self.ball.v = V3::new(0.0, 0.0, 0.0);
        self.trail.clear();
        self.bounces = 0;
        self.net_touched = false;
        self.is_serve = false;
        self.last_hitter = 2;
        self.rally = 0;
    }

    fn do_serve(&mut self, server: u8) {
        let (sx, sz) = if server == 0 {
            (self.px.clamp(-3.5, 3.5), self.pz.min(0.4))
        } else {
            (self.ax, self.az)
        };
        let y0 = 2.45;
        let vy = 2.7 + self.rnd() * 0.5;
        let (tx, tz) = if server == 0 {
            (
                self.dir as f64 * 2.4 + (self.rnd() - 0.5) * 1.6,
                NET_Z + 2.5 + self.rnd() * 8.0,
            )
        } else {
            let corner = if self.px > 0.3 {
                -1.0
            } else if self.px < -0.3 {
                1.0
            } else if self.rnd() < 0.5 {
                -1.0
            } else {
                1.0
            };
            (
                corner * (1.2 + self.rnd() * 2.2) + (self.rnd() - 0.5),
                2.0 + self.rnd() * 8.5,
            )
        };
        let t = (vy + (vy * vy + 2.0 * GRAVITY * y0).sqrt()) / GRAVITY;
        self.ball.p = V3::new(sx, y0, sz);
        self.ball.v = V3::new((tx - sx) / t, vy, (tz - sz) / t);
        self.ball.live = true;
        self.is_serve = true;
        self.last_hitter = server;
        self.bounces = 0;
        self.net_touched = false;
        self.phase = Phase::Rally;
    }

    /// Solve a flat-ish arc that lands exactly at (tx, tz).
    fn stroke(&mut self, who: u8, tx: f64, tz: f64, vy: f64) {
        let p = self.ball.p;
        let y0 = p.y.max(0.3);
        let t = (vy + (vy * vy + 2.0 * GRAVITY * y0).sqrt()) / GRAVITY;
        self.ball.v = V3::new((tx - p.x) / t, vy, (tz - p.z) / t);
        self.last_hitter = who;
        self.bounces = 0;
        self.net_touched = false;
        self.is_serve = false;
        self.rally += 1;
        self.spawn_dust(p, 3, 2.6);
    }

    fn try_hits(&mut self) {
        // you — auto-swing when the ball is in reach (after a bounce)
        let ok = self.ball.live
            && self.last_hitter != 0
            && self.bounces >= 1
            && self.ball.v.z < 0.0
            && self.ball.p.z < self.pz + 0.9
            && self.ball.p.z > self.pz - 1.7
            && (self.ball.p.x - self.px).abs() < 1.3
            && self.ball.p.y < 2.5;
        if ok {
            let quality = 1.0 - ((self.ball.p.x - self.px).abs() / 1.3).min(1.0);
            let mut tx = self.dir as f64 * (HALF_W - 0.6) + (self.ball.p.x - self.px) * 1.4;
            tx += (self.rnd() - 0.5) * 2.4 * (1.0 - quality); // stretched = sloppy
            let tx = tx.clamp(-(HALF_W + 0.4), HALF_W + 0.4);
            let tz = COURT_LEN - 1.6 - self.rnd() * 3.5 - quality * 2.0;
            let vy = 3.6 + self.rnd() * 0.7 + self.ball.p.y * 0.22;
            self.stroke(0, tx, tz, vy);
        }
        // cpu — aims away from you, occasionally overhits
        let ok = self.ball.live
            && self.last_hitter != 1
            && self.bounces >= 1
            && self.ball.v.z > 0.0
            && self.ball.p.z > self.az - 0.9
            && self.ball.p.z < self.az + 1.7
            && (self.ball.p.x - self.ax).abs() < 1.25
            && self.ball.p.y < 2.5;
        if ok {
            let away = if self.px > 0.0 { -1.0 } else { 1.0 };
            let tx = away * (HALF_W - 1.1) + (self.rnd() - 0.5) * 2.6;
            let tx = tx.clamp(-(HALF_W + 0.5), HALF_W + 0.5);
            let tz = 1.6 + self.rnd() * 4.5;
            let x = self.rnd() * 0.9;
            self.stroke(1, tx, tz, 3.3 + x);
        }
    }

    fn physics(&mut self, dt: f64) {
        if !self.ball.live {
            return;
        }
        let prev = self.ball.p;
        self.ball.v.y -= GRAVITY * dt;
        self.ball.p.x += self.ball.v.x * dt;
        self.ball.p.y += self.ball.v.y * dt;
        self.ball.p.z += self.ball.v.z * dt;

        // net crossing
        if (prev.z - NET_Z).signum() != (self.ball.p.z - NET_Z).signum() {
            let denom = self.ball.p.z - prev.z;
            if denom.abs() > 1e-9 {
                let t = (NET_Z - prev.z) / denom;
                let yc = prev.y + (self.ball.p.y - prev.y) * t;
                let xc = prev.x + (self.ball.p.x - prev.x) * t;
                if yc < NET_H && xc.abs() < DOUBLES_W + 0.55 {
                    self.net_touched = true;
                    if yc > NET_H - 0.22 {
                        // clipped the tape
                        self.ball.v.z *= 0.32;
                        self.ball.v.y = self.ball.v.y.abs() * 0.15 + 0.35;
                        self.ball.v.x *= 0.55;
                        if self.phase == Phase::Rally {
                            self.flash("NET CORD!", pal::INFO, 0.7);
                        }
                    } else {
                        // into the mesh
                        let dir = denom.signum();
                        self.ball.p.z = NET_Z - dir * 0.12;
                        self.ball.v.z *= -0.05;
                        self.ball.v.x *= 0.35;
                        if self.ball.v.y > 0.0 {
                            self.ball.v.y = 0.0;
                        }
                    }
                }
            }
        }

        // ground
        if self.ball.p.y <= 0.05 && self.ball.v.y < 0.0 {
            self.ball.p.y = 0.05;
            self.ball.v.y = -self.ball.v.y * 0.58;
            if self.ball.v.y < 0.4 {
                self.ball.v.y = 0.0;
            }
            self.ball.v.x *= 0.9;
            self.ball.v.z *= 0.94;
            self.dust_burst(self.ball.p);
            self.on_bounce();
        }

        if self.frame % 2 == 0 {
            self.trail.insert(0, self.ball.p);
            self.trail.truncate(9);
        }
        if self.ball.p.z < -9.0 || self.ball.p.z > COURT_LEN + 10.0 || self.ball.p.x.abs() > 18.0 {
            if self.phase == Phase::Rally {
                let loser = if self.last_hitter == 0 { 0 } else { 1 };
                self.award(1 - loser, "OUT!");
            }
        }
    }

    fn on_bounce(&mut self) {
        if self.phase != Phase::Rally {
            return;
        }
        self.bounces += 1;
        let hitter = self.last_hitter;
        let near_side = self.ball.p.z < NET_Z;
        let in_bounds = self.ball.p.x.abs() <= HALF_W + 0.15
            && self.ball.p.z >= -0.15
            && self.ball.p.z <= COURT_LEN + 0.15;
        let own_side = (hitter == 0 && near_side) || (hitter == 1 && !near_side);

        if self.bounces == 1 {
            if own_side || !in_bounds {
                if self.is_serve {
                    self.score.serves -= 1;
                    if self.score.serves == 0 {
                        self.award(1 - hitter as usize, "DOUBLE FAULT");
                    } else {
                        self.flash("FAULT", pal::INFO, 1.0);
                        self.phase = Phase::Point(0.9);
                    }
                } else {
                    let what = if own_side { "NET!" } else { "OUT!" };
                    self.award(1 - hitter as usize, what);
                }
            }
        } else {
            let what = if self.is_serve { "ACE!" } else { "WINNER!" };
            self.award(hitter as usize, what);
        }
    }

    fn award(&mut self, winner: usize, what: &str) {
        let loser = 1 - winner;
        self.score.pts[winner] += 1;
        self.score.serves = 2;
        let (pw, pl) = (self.score.pts[winner], self.score.pts[loser]);
        let mut over = false;
        let mut sub = String::new();
        if pw >= 4 && pw >= pl + 2 {
            self.score.pts = [0, 0];
            self.score.games[winner] += 1;
            self.score.server = 1 - self.score.server;
            if self.score.games[winner] >= GAMES_TO_WIN {
                over = true;
            } else {
                sub = format!("GAME  ·  {}–{}", self.score.games[0], self.score.games[1]);
            }
        } else {
            sub = format!(
                "{} – {}",
                pt_str(self.score.pts[0]),
                pt_str(self.score.pts[1])
            );
            if self.score.pts[0] >= 3 && self.score.pts[1] >= 3 {
                sub = if self.score.pts[0] == self.score.pts[1] {
                    "DEUCE".into()
                } else {
                    format!(
                        "ADVANTAGE {}",
                        if self.score.pts[0] > self.score.pts[1] {
                            "YOU"
                        } else {
                            "CPU"
                        }
                    )
                };
            }
        }
        let color = if winner == 0 { pal::GOOD } else { pal::BAD };
        self.msg = Some((what.to_string(), color, 1.6));
        self.sub = Some((sub, 2.0));
        if over {
            self.msg = Some((
                if winner == 0 {
                    "GAME, SET & MATCH — YOU!"
                } else {
                    "GAME, SET & MATCH — CPU"
                }
                .into(),
                color,
                99.0,
            ));
            self.phase = Phase::Over;
        } else {
            self.phase = Phase::Point(1.7);
        }
    }
    fn spawn_dust(&mut self, at: V3, count: usize, spread: f64) {
        for _ in 0..count {
            let v = V3::new(
                (self.rnd() - 0.5) * spread,
                self.rnd() * spread * 0.75,
                (self.rnd() - 0.5) * spread,
            );
            let life = 0.25 + self.rnd() as f32 * 0.2;
            self.dust.push(Particle { p: at, v, life });
        }
        if self.dust.len() > 80 {
            self.dust.drain(0..20);
        }
    }

    fn dust_burst(&mut self, p: V3) {
        self.spawn_dust(V3::new(p.x, 0.05, p.z), 6, 4.4);
    }

    fn move_player(&mut self, dt: f64) {
        self.px = (self.px + self.dir as f64 * 6.4 * dt).clamp(-6.2, 6.2);
        self.pz = (self.pz + self.vdir as f64 * 3.4 * dt).clamp(-0.5, 4.2);
    }

    fn move_ai(&mut self, dt: f64) {
        let incoming = self.ball.live && self.ball.v.z > 0.0 && self.last_hitter != 1;
        let target = if incoming {
            let t = (self.az - self.ball.p.z) / self.ball.v.z;
            if t > 0.0 {
                (self.ball.p.x + self.ball.v.x * t).clamp(-6.0, 6.0)
            } else {
                self.ball.p.x
            }
        } else {
            self.ball.p.x * 0.25
        };
        let sp = 5.1 + self.score.games[1] as f64 * 0.45; // cpu speeds up as it wins games
        self.ax += (target - self.ax).clamp(-sp * dt, sp * dt);
    }

    fn step(&mut self, dt: f64) {
        self.frame += 1;
        if self.paused {
            return;
        }
        if self.dir_ttl > 0 {
            self.dir_ttl -= 1;
        } else {
            self.dir = 0;
        }
        if self.vdir_ttl > 0 {
            self.vdir_ttl -= 1;
        } else {
            self.vdir = 0;
        }
        if let Some((_, _, t)) = &mut self.msg {
            *t -= dt as f32;
            if *t <= 0.0 {
                self.msg = None;
            }
        }
        if let Some((_, t)) = &mut self.sub {
            *t -= dt as f32;
            if *t <= 0.0 {
                self.sub = None;
            }
        }

        for d in &mut self.dust {
            d.life -= dt as f32;
            d.v.y -= 5.0 * dt;
            d.p.x += d.v.x * dt;
            d.p.y += d.v.y * dt;
            d.p.z += d.v.z * dt;
            if d.p.y < 0.03 {
                d.p.y = 0.03;
                d.v.y = 0.0;
            }
        }
        self.dust.retain(|d| d.life > 0.0);

        match self.phase {
            Phase::Serve { server, mut wait } => {
                self.move_player(dt);
                self.ax += ((self.frame as f64 * 0.02).sin() * 1.4 - self.ax) * 0.02;
                self.ball.p = if server == 0 {
                    V3::new(self.px, 1.1, self.pz)
                } else {
                    V3::new(self.ax, 1.1, self.az)
                };
                wait -= dt as f32;
                if server == 1 && wait <= 0.0 {
                    self.do_serve(1);
                } else {
                    self.phase = Phase::Serve { server, wait };
                }
            }
            Phase::Rally => {
                self.move_player(dt);
                self.move_ai(dt);
                self.physics(dt);
                self.try_hits();
            }
            Phase::Point(mut t) => {
                self.physics(dt);
                t -= dt as f32;
                if t <= 0.0 {
                    self.begin_serve();
                } else {
                    self.phase = Phase::Point(t);
                }
            }
            Phase::Over => self.physics(dt),
        }
    }
}

// ── rendering ────────────────────────────────────────────────────────────────
fn hash2(x: u64, y: u64, seed: u64) -> u64 {
    let mut h =
        x.wrapping_mul(0x9E3779B1) ^ y.wrapping_mul(0x85EBCA77) ^ seed.wrapping_mul(0xC2B2AE3D);
    h ^= h >> 13;
    h = h.wrapping_mul(0x27D4EB2F);
    h ^ (h >> 15)
}
fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

fn plot(buf: &mut Buffer, cam: &Cam, p: V3, ch: char, style: Style) {
    if let Some((sx, sy)) = cam.proj(p) {
        let r = buf.area();
        let (x, y) = (sx.round() as i64, sy.round() as i64);
        if x >= r.x as i64
            && x < (r.x + r.width) as i64
            && y >= r.y as i64
            && y < (r.y + r.height) as i64
        {
            if let Some(cell) = buf.cell_mut((x as u16, y as u16)) {
                cell.set_char(ch);
                cell.set_style(style);
            }
        }
    }
}

fn line3(buf: &mut Buffer, cam: &Cam, a: V3, b: V3, style: Style, steps: usize) {
    let (Some(pa), Some(pb)) = (cam.proj(a), cam.proj(b)) else {
        return;
    };
    let (dx, dy) = (pb.0 - pa.0, pb.1 - pa.1);
    let ch = if dy.abs() < 1.2 && dx.abs() >= dy.abs() {
        '─'
    } else if dx.abs() < 1.2 {
        '│'
    } else if dx * dy < 0.0 {
        '╱'
    } else {
        '╲'
    };
    for i in 0..=steps {
        let t = i as f64 / steps as f64;
        plot(
            buf,
            cam,
            V3::new(
                a.x + (b.x - a.x) * t,
                a.y + (b.y - a.y) * t,
                a.z + (b.z - a.z) * t,
            ),
            ch,
            style,
        );
    }
}

fn ground_cell(x: f64, z: f64) -> (char, Style) {
    if z < -2.2 || z > COURT_LEN + 3.4 || x.abs() > DOUBLES_W + 2.6 {
        return (' ', Style::default().bg(pal::GROUND));
    }
    let stripe = ((z / 1.9).floor() as i64).rem_euclid(2) as usize;
    let inside = x.abs() <= HALF_W && (0.0..=COURT_LEN).contains(&z);
    let bg = if inside {
        pal::COURT[stripe]
    } else {
        pal::APRON[stripe]
    };
    (' ', Style::default().bg(bg))
}

fn paint_background(buf: &mut Buffer, area: Rect, cam: &Cam, frame: u64) {
    let horizon = cam.horizon.round() as i64;
    for row in area.y..area.y + area.height {
        let ri = row as i64;
        if ri < horizon {
            let t = (ri - area.y as i64).max(0) as f64 / (horizon - area.y as i64).max(1) as f64;
            let sky = Rgb(
                lerp(7.0, 24.0, t) as u8,
                lerp(10.0, 32.0, t) as u8,
                lerp(22.0, 58.0, t) as u8,
            );
            let in_stands = ri >= horizon - 3;
            for col in area.x..area.x + area.width {
                if let Some(c) = buf.cell_mut((col, row)) {
                    if in_stands {
                        let h = hash2(col as u64, ri as u64, frame / 14); // crowd twinkles
                        if h % 11 == 0 {
                            let crowd = [
                                Rgb(214, 90, 90),
                                Rgb(230, 190, 90),
                                Rgb(120, 170, 230),
                                Rgb(220, 220, 230),
                                Rgb(140, 200, 140),
                            ];
                            c.set_char(' ');
                            c.set_style(
                                Style::default().bg(crowd[(h as usize / 11) % crowd.len()]),
                            );
                        } else {
                            c.set_char('·');
                            c.set_style(Style::default().fg(Rgb(44, 50, 68)).bg(Rgb(15, 19, 29)));
                        }
                    } else if hash2(col as u64, ri as u64, 7) % 97 == 0 {
                        c.set_char('·');
                        c.set_style(Style::default().fg(Rgb(190, 200, 245)).bg(sky));
                    } else {
                        c.set_char(' ');
                        c.set_style(Style::default().bg(sky));
                    }
                }
            }
        } else {
            let z = cam.z_at_row(ri as f64 + 0.5);
            let d = z + cam.dist;
            for col in area.x..area.x + area.width {
                let xw = cam.x_at_col(col as f64 + 0.5, d);
                let (ch, style) = ground_cell(xw, z);
                if let Some(c) = buf.cell_mut((col, row)) {
                    c.set_char(ch);
                    c.set_style(style);
                }
            }
        }
    }
    // floodlights
    if area.width > 14 {
        for fx in [area.x + 3, area.x + area.width - 4] {
            buf.set_string(
                fx,
                area.y + 1,
                "✦",
                Style::default().fg(Rgb(255, 248, 214)).bold(),
            );
        }
    }
    // sponsor wall behind the far baseline
    const BANNER: &[u8] = b"  RATATUI OPEN * TERMINAL TENNIS *";
    let zw = COURT_LEN + 1.6;
    let d = zw + cam.dist;
    for col in area.x..area.x + area.width {
        let xw = cam.x_at_col(col as f64 + 0.5, d);
        if xw.abs() > 13.5 {
            continue;
        }
        let base = cam.proj(V3::new(xw, 0.0, zw)).unwrap().1.round() as i64;
        let top = cam.proj(V3::new(xw, 2.7, zw)).unwrap().1.round() as i64;
        let mid = (base + top) / 2;
        for row in top..=base {
            if row < area.y as i64 || row >= (area.y + area.height) as i64 {
                continue;
            }
            if let Some(c) = buf.cell_mut((col, row as u16)) {
                if row == top {
                    c.set_char(' ');
                    c.set_style(Style::default().bg(Rgb(64, 84, 138)));
                } else if row == mid {
                    c.set_char(BANNER[(col as usize).rem_euclid(BANNER.len())] as char);
                    c.set_style(
                        Style::default()
                            .fg(Rgb(165, 190, 245))
                            .bg(pal::WALL)
                            .add_modifier(Modifier::BOLD),
                    );
                } else {
                    c.set_char(' ');
                    c.set_style(Style::default().bg(pal::WALL));
                }
            }
        }
    }
}

fn paint_court_lines(buf: &mut Buffer, cam: &Cam) {
    let w = Style::default().fg(pal::LINE).add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Rgb(190, 205, 228));
    line3(
        buf,
        cam,
        V3::new(-HALF_W, 0.0, 0.0),
        V3::new(HALF_W, 0.0, 0.0),
        w,
        48,
    );
    line3(
        buf,
        cam,
        V3::new(-HALF_W, 0.0, COURT_LEN),
        V3::new(HALF_W, 0.0, COURT_LEN),
        w,
        48,
    );
    line3(
        buf,
        cam,
        V3::new(-HALF_W, 0.0, 0.0),
        V3::new(-HALF_W, 0.0, COURT_LEN),
        w,
        72,
    );
    line3(
        buf,
        cam,
        V3::new(HALF_W, 0.0, 0.0),
        V3::new(HALF_W, 0.0, COURT_LEN),
        w,
        72,
    );
    line3(
        buf,
        cam,
        V3::new(-DOUBLES_W, 0.0, 0.0),
        V3::new(-DOUBLES_W, 0.0, COURT_LEN),
        dim,
        72,
    );
    line3(
        buf,
        cam,
        V3::new(DOUBLES_W, 0.0, 0.0),
        V3::new(DOUBLES_W, 0.0, COURT_LEN),
        dim,
        72,
    );
    line3(
        buf,
        cam,
        V3::new(-HALF_W, 0.0, SVC_NEAR),
        V3::new(HALF_W, 0.0, SVC_NEAR),
        w,
        40,
    );
    line3(
        buf,
        cam,
        V3::new(-HALF_W, 0.0, SVC_FAR),
        V3::new(HALF_W, 0.0, SVC_FAR),
        w,
        40,
    );
    line3(
        buf,
        cam,
        V3::new(0.0, 0.0, SVC_NEAR),
        V3::new(0.0, 0.0, SVC_FAR),
        w,
        40,
    );
    line3(
        buf,
        cam,
        V3::new(0.0, 0.0, 0.0),
        V3::new(0.0, 0.0, 0.18),
        w,
        4,
    );
    line3(
        buf,
        cam,
        V3::new(0.0, 0.0, COURT_LEN),
        V3::new(0.0, 0.0, COURT_LEN - 0.18),
        w,
        4,
    );
}

fn paint_net(buf: &mut Buffer, cam: &Cam) {
    let r = buf.area().clone(); // copy the Rect out, borrow ends immediately
    let post_x = DOUBLES_W + 0.55;
    let mut x = -post_x;
    while x <= post_x {
        let base = cam.proj(V3::new(x, 0.0, NET_Z));
        let top = cam.proj(V3::new(x, NET_H, NET_Z));
        if let (Some((sx, yb)), Some((_, yt))) = (base, top) {
            let c = sx.round() as i64;
            if c >= r.x as i64 && c < (r.x + r.width) as i64 {
                for row in (yt.round() as i64)..=(yb.round() as i64) {
                    if row < r.y as i64 || row >= (r.y + r.height) as i64 {
                        continue;
                    }
                    if let Some(cell) = buf.cell_mut((c as u16, row as u16)) {
                        cell.set_char('╽');
                        cell.set_style(Style::default().fg(Rgb(150, 160, 185)));
                    }
                }
            }
        }
        x += 0.34;
    }
    line3(
        buf,
        cam,
        V3::new(-post_x, NET_H, NET_Z),
        V3::new(post_x, NET_H, NET_Z),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
        60,
    );
    for s in [-1.0, 1.0] {
        line3(
            buf,
            cam,
            V3::new(s * post_x, 0.0, NET_Z),
            V3::new(s * post_x, NET_H + 0.12, NET_Z),
            Style::default()
                .fg(Rgb(170, 180, 200))
                .add_modifier(Modifier::BOLD),
            8,
        );
    }
}

fn paint_figure(buf: &mut Buffer, cam: &Cam, x: f64, z: f64, color: Color, frame: u64) {
    let bob = (frame as f64 * 0.12 + x).sin() * 0.05;
    plot(
        buf,
        cam,
        V3::new(x, 0.02, z),
        '●',
        Style::default().fg(Rgb(8, 12, 22)),
    );
    plot(
        buf,
        cam,
        V3::new(x, 0.52 + bob, z),
        '▲',
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    );
    plot(
        buf,
        cam,
        V3::new(x, 1.58 + bob, z),
        'o',
        Style::default().fg(Rgb(238, 206, 172)),
    );
    for i in 0..20 {
        let a = i as f64 / 20.0 * 2.0 * PI;
        plot(
            buf,
            cam,
            V3::new(x + 0.42 * a.cos(), 1.05 + 0.55 * a.sin() + bob, z + 0.06),
            'O',
            Style::default().fg(color),
        );
    }
    plot(
        buf,
        cam,
        V3::new(x, 1.05 + bob, z + 0.06),
        '·',
        Style::default().fg(Rgb(220, 226, 240)),
    );
    plot(
        buf,
        cam,
        V3::new(x, 0.46 + bob, z + 0.06),
        '│',
        Style::default().fg(Rgb(126, 88, 54)),
    );
}

fn paint_ball(buf: &mut Buffer, cam: &Cam, g: &Game) {
    if g.ball.live {
        let n = g.trail.len().max(1) as f64;
        for (i, p) in g.trail.iter().enumerate().rev() {
            let fade = 1.0 - i as f64 / n;
            plot(
                buf,
                cam,
                *p,
                if i % 2 == 0 { '·' } else { '∙' },
                Style::default().fg(Rgb(
                    (50.0 + 150.0 * fade) as u8,
                    (80.0 + 165.0 * fade) as u8,
                    40,
                )),
            );
        }
    }
    plot(
        buf,
        cam,
        V3::new(g.ball.p.x, 0.02, g.ball.p.z),
        '●',
        Style::default().fg(Rgb(10, 15, 28)),
    );
    plot(
        buf,
        cam,
        g.ball.p,
        '●',
        Style::default().fg(pal::BALL).add_modifier(Modifier::BOLD),
    );
}

fn paint_actors(buf: &mut Buffer, cam: &Cam, g: &Game, near: bool) {
    let side = |z: f64| if near { z <= NET_Z } else { z > NET_Z };
    if !near {
        paint_figure(buf, cam, g.ax, g.az, pal::CPU, g.frame);
    }
    if side(g.ball.p.z) {
        paint_ball(buf, cam, g);
    }
    for d in &g.dust {
        if side(d.p.z) {
            plot(
                buf,
                cam,
                d.p,
                if d.life > 0.18 { '*' } else { '·' },
                Style::default().fg(Rgb(165, 195, 230)),
            );
        }
    }
    if near {
        paint_figure(buf, cam, g.px, g.pz, pal::YOU, g.frame);
    }
}

fn center_text(buf: &mut Buffer, area: Rect, row: u16, text: &str, style: Style) {
    let w = text.chars().count() as u16;
    if w >= area.width || row < area.y || row >= area.y + area.height {
        return;
    }
    buf.set_string(area.x + (area.width - w) / 2, row, text, style);
}

fn paint_hud(buf: &mut Buffer, area: Rect, cam: &Cam, g: &Game) {
    if let Phase::Serve { server, .. } = g.phase {
        let pulse = (g.frame as f64 * 0.18).sin() * 0.5 + 0.5;
        if server == 0 {
            let (tx, tz) = (g.dir as f64 * 2.4, NET_Z + 6.5);
            let col = if pulse > 0.45 {
                pal::BALL
            } else {
                Rgb(110, 130, 60)
            };
            plot(
                buf,
                cam,
                V3::new(tx, 0.03, tz),
                '×',
                Style::default().fg(col).add_modifier(Modifier::BOLD),
            );
            for (ox, oz) in [(0.7, 0.0), (-0.7, 0.0), (0.0, 0.9), (0.0, -0.9)] {
                plot(
                    buf,
                    cam,
                    V3::new(tx + ox, 0.03, tz + oz),
                    '·',
                    Style::default().fg(Rgb(150, 165, 90)),
                );
            }
        }
        let prompt = if server == 0 {
            "SPACE · SERVE"
        } else {
            "CPU SERVING…"
        };
        center_text(
            buf,
            area,
            area.y + area.height - 2,
            prompt,
            Style::default().fg(pal::INFO).bold(),
        );
    }
    if g.rally >= 4 && g.phase == Phase::Rally {
        let t = format!("RALLY {}", g.rally);
        buf.set_string(
            area.x + area.width.saturating_sub(t.len() as u16 + 2),
            area.y + 1,
            t,
            Style::default().fg(Rgb(255, 205, 100)).bold(),
        );
    }
    let my = area.y + (area.height as f64 * 0.34) as u16;
    if let Some((m, c, _)) = &g.msg {
        center_text(
            buf,
            area,
            my,
            m,
            Style::default().fg(*c).add_modifier(Modifier::BOLD),
        );
    }
    if let Some((s, _)) = &g.sub {
        center_text(
            buf,
            area,
            my + 2,
            s,
            Style::default().fg(Rgb(224, 230, 246)).bold(),
        );
    }
}

struct CourtView<'a> {
    game: &'a Game,
}

impl Widget for CourtView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width < 20 || area.height < 8 {
            return;
        }
        let cam = Cam::for_rect(area);
        paint_background(buf, area, &cam, self.game.frame);
        paint_court_lines(buf, &cam);
        paint_actors(buf, &cam, self.game, false); // far side…
        paint_net(buf, &cam); // …then net…
        paint_actors(buf, &cam, self.game, true); // …then near side
        paint_hud(buf, area, &cam, self.game);
    }
}

// ── scoreboard ───────────────────────────────────────────────────────────────

fn pips(won: u8) -> String {
    (0..GAMES_TO_WIN)
        .map(|i| if i < won { '■' } else { '□' })
        .collect()
}

fn situation(s: &Score) -> (String, Color) {
    let (a, b) = (s.pts[0], s.pts[1]);
    if a >= 3 && b >= 3 {
        if a == b {
            return ("DEUCE".to_string(), pal::INFO);
        }
        let i = if a > b { 0usize } else { 1usize };
        let name = if i == 0 { "YOU" } else { "CPU" };
        let label = if s.games[i] + 1 >= GAMES_TO_WIN {
            "ADVANTAGE · MATCH POINT"
        } else if s.server as usize != i {
            "ADVANTAGE · BREAK POINT"
        } else {
            "ADVANTAGE"
        };
        let c = if i == 0 { pal::GOOD } else { pal::BAD };
        return (format!("{label} — {name}"), c);
    }
    for i in 0..2usize {
        let j = 1 - i;
        if s.pts[i] >= 3 && s.pts[i] > s.pts[j] {
            let name = if i == 0 { "YOU" } else { "CPU" };
            let label = if s.games[i] + 1 >= GAMES_TO_WIN {
                "MATCH POINT"
            } else if s.server as usize != i {
                "BREAK POINT"
            } else {
                "GAME POINT"
            };
            let c = if i == 0 { pal::GOOD } else { pal::BAD };
            return (format!("{label} — {name}"), c);
        }
    }
    let server = if s.server == 0 { "YOU" } else { "CPU" };
    (
        format!(
            "GAME {} OF {} · {} SERVING",
            s.games[0] + s.games[1] + 1,
            2 * GAMES_TO_WIN - 1,
            server
        ),
        Rgb(120, 132, 160),
    )
}

fn score_row(g: &Game, i: usize) -> Line<'static> {
    let s = &g.score;
    let serving = s.server as usize == i;
    let name = if i == 0 { "YOU" } else { "CPU" };
    let color = if i == 0 { pal::YOU } else { pal::CPU };
    Line::from(vec![
        Span::styled(
            if serving && g.frame % 64 < 40 {
                " ● "
            } else {
                "   "
            },
            Style::default().fg(pal::BALL),
        ),
        Span::styled(
            format!("{name:<4}"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:>3}", pt_label(s.pts[i], s.pts[1 - i])),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("    "),
        Span::styled(
            pips(s.games[i]),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("GAMES {}", s.games[i]),
            Style::default().fg(Rgb(104, 116, 144)),
        ),
    ])
}

fn scoreboard(g: &Game, width: u16) -> Paragraph<'static> {
    let v = &g.ball.v;
    let speed = (v.x * v.x + v.y * v.y + v.z * v.z).sqrt() * 3.6;
    let right = match g.phase {
        Phase::Serve { server, .. } => {
            format!("SERVICE — {} ", if server == 0 { "YOU" } else { "CPU" })
        }
        Phase::Over => "MATCH OVER ".to_string(),
        _ if g.ball.live && speed > 8.0 => {
            format!("RALLY {:>2} · {:>3.0} KM/H ", g.rally.max(1), speed)
        }
        _ => "FIRST TO 3 GAMES ".to_string(),
    };
    let mut header = vec![
        Span::styled(
            " RATATUI OPEN",
            Style::default()
                .fg(Rgb(150, 182, 255))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · CENTRE COURT", Style::default().fg(Rgb(88, 100, 130))),
    ];
    let left_len: usize = header.iter().map(|s| s.content.chars().count()).sum();
    let pad = (width as usize).saturating_sub(left_len + right.chars().count());
    header.push(Span::raw(" ".repeat(pad)));
    header.push(Span::styled(
        right,
        Style::default()
            .fg(Rgb(255, 214, 120))
            .add_modifier(Modifier::BOLD),
    ));

    let (status_text, status_color) = situation(&g.score);

    Paragraph::new(vec![
        Line::from(header),
        score_row(g, 0),
        score_row(g, 1),
        Line::from(vec![
            Span::raw("   "),
            Span::styled(
                status_text,
                Style::default()
                    .fg(status_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
    ])
    .style(Style::default().bg(Rgb(9, 13, 25)))
}
fn bottom_bar() -> Paragraph<'static> {
    let key = |k: &'static str| {
        Span::styled(
            format!(" {k} "),
            Style::default()
                .fg(Rgb(230, 236, 250))
                .add_modifier(Modifier::BOLD),
        )
    };
    let label = |l: &'static str| Span::styled(l, Style::default().fg(Rgb(96, 106, 130)));
    Paragraph::new(Line::from(vec![
        key("◀ ▶"),
        label(" move "),
        key("▲ ▼"),
        label(" depth "),
        key("SPACE"),
        label(" serve "),
        key("P"),
        label(" pause "),
        key("R"),
        label(" rematch "),
        key("Q"),
        label(" quit "),
        Span::raw("      "),
        Span::styled(
            "centre court · 60 fps · ratatui",
            Style::default().fg(Rgb(70, 80, 105)),
        ),
    ]))
    .style(Style::default().bg(Rgb(10, 14, 26)))
}

fn overlay(f: &mut Frame, area: Rect, title: &str, lines: Vec<Line<'static>>, color: Color) {
    let w = 46u16.min(area.width.saturating_sub(2));
    let h = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
    if w < 12 || h < 3 {
        return;
    }
    let rect = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(color))
                    .title(format!(" {title} ")),
            )
            .style(Style::default().bg(Rgb(11, 15, 27)))
            .alignment(Alignment::Center),
        rect,
    );
}

fn draw(f: &mut Frame, g: &Game) {
    let area = f.area();
    let rows = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(6),
        Constraint::Length(2),
    ])
    .split(area);
    f.render_widget(scoreboard(g, rows[0].width), rows[0]);
    f.render_widget(CourtView { game: g }, rows[1]);
    f.render_widget(bottom_bar(), rows[2]);
    if g.paused {
        overlay(
            f,
            rows[1],
            "PAUSED",
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    "press P to resume",
                    Style::default().fg(Rgb(200, 210, 235)),
                )),
            ],
            Rgb(120, 140, 190),
        );
    }
    if let Phase::Over = g.phase {
        let you_win = g.score.games[0] > g.score.games[1];
        let c = if you_win { pal::GOOD } else { pal::BAD };
        overlay(
            f,
            rows[1],
            "MATCH OVER",
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    if you_win {
                        ">> YOU WIN THE MATCH <<"
                    } else {
                        "CPU TAKES THE MATCH"
                    },
                    Style::default().fg(c).add_modifier(Modifier::BOLD),
                )),
                Line::from(format!("games {}–{}", g.score.games[0], g.score.games[1])),
                Line::from(Span::styled(
                    "R — rematch · Q — quit",
                    Style::default().fg(Rgb(150, 160, 185)),
                )),
            ],
            c,
        );
    }
}

// ── main loop ────────────────────────────────────────────────────────────────
fn run(terminal: &mut DefaultTerminal) -> std::io::Result<()> {
    let mut game = Game::new();
    let mut last = Instant::now();
    let mut acc = 0.0;
    loop {
        let now = Instant::now();
        acc += (now - last).as_secs_f64().min(0.1);
        last = now;
        while acc >= DT {
            game.step(DT);
            acc -= DT;
        }
        terminal.draw(|f| draw(f, &game))?;

        let timeout = Duration::from_secs_f64((DT - acc).max(0.0));
        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                match key.kind {
                    KeyEventKind::Press | KeyEventKind::Repeat => {
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                            && key.code == KeyCode::Char('c')
                        {
                            break;
                        }
                        match key.code {
                            KeyCode::Left | KeyCode::Char('a') => {
                                game.dir = -1;
                                game.dir_ttl = KEY_HOLD_FRAMES;
                            }
                            KeyCode::Right | KeyCode::Char('d') => {
                                game.dir = 1;
                                game.dir_ttl = KEY_HOLD_FRAMES;
                            }
                            KeyCode::Up | KeyCode::Char('w') => {
                                game.vdir = 1;
                                game.vdir_ttl = KEY_HOLD_FRAMES;
                            }
                            KeyCode::Down | KeyCode::Char('s') => {
                                game.vdir = -1;
                                game.vdir_ttl = KEY_HOLD_FRAMES;
                            }
                            KeyCode::Char(' ') => {
                                if let Phase::Serve { server: 0, .. } = game.phase {
                                    game.do_serve(0);
                                }
                            }
                            KeyCode::Char('p') => game.paused = !game.paused,
                            KeyCode::Char('r') => game = Game::new(),
                            KeyCode::Char('q') | KeyCode::Esc => break,
                            _ => {}
                        }
                    }
                    KeyEventKind::Release => match key.code {
                        KeyCode::Left | KeyCode::Char('a') => {
                            if game.dir == -1 {
                                game.dir = 0;
                                game.dir_ttl = 0;
                            }
                        }
                        KeyCode::Right | KeyCode::Char('d') => {
                            if game.dir == 1 {
                                game.dir = 0;
                                game.dir_ttl = 0;
                            }
                        }
                        KeyCode::Up | KeyCode::Char('w') => {
                            if game.vdir == 1 {
                                game.vdir = 0;
                                game.vdir_ttl = 0;
                            }
                        }
                        KeyCode::Down | KeyCode::Char('s') => {
                            if game.vdir == -1 {
                                game.vdir = 0;
                                game.vdir_ttl = 0;
                            }
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    let mut terminal = ratatui::init();
    let result = run(&mut terminal);
    ratatui::restore();
    result
}
