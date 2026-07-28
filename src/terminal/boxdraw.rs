//! Native box-drawing: the U+2500–U+257F box characters and U+2580–U+259F
//! block elements, drawn as geometry sized to the actual cell instead of as
//! font glyphs.
//!
//! Why the font can't do this job: a glyph fills (at most) the font's own line
//! height, but the cell it paints into is `font_size × Config::line_height` —
//! 1.4 by default. At any line height above 1.0 a `│` covers only the middle of
//! its cell, so every vertical run of box characters breaks into dashes with a
//! gap at each row boundary: a two-line shell prompt's `╭`/`╰` no longer
//! connect, a TUI frame is perforated down both sides. Horizontal continuity
//! has the same problem in miniature whenever a fallback face's advance
//! disagrees with the cell width.
//!
//! Drawing the range natively pins every stroke to the cell's real edges, so
//! adjacent cells join seamlessly at any line height, any font, any fallback
//! chain. This is the same special case every terminal with a line-height
//! setting ships (kitty, alacritty, WezTerm, iTerm2), and the same approach the
//! Powerline separators in `element.rs` already use — they skip fonts entirely.
//!
//! [`glyph`] returns the character's ink as rectangles and filled paths in cell
//! coordinates; `paint_glyphs` fills them with the cell's foreground. A char
//! outside the range returns `None` and falls back to the font.

use gpui::{Bounds, Pixels, point, px, size};

/// One paintable piece of a box-drawing glyph.
pub(crate) enum Ink {
    /// A solid rectangle in the cell's foreground color.
    Rect(Bounds<Pixels>),
    /// A rectangle at a fraction of the foreground's alpha — the ░▒▓ shades,
    /// which fake their dither by translucency exactly as WezTerm does.
    Shade(Bounds<Pixels>, f32),
    /// A filled path — rounded corners and diagonals, the two shapes a
    /// rectangle can't express.
    Path(gpui::Path<Pixels>),
}

/// The ink for `c` sized to `bounds`, or `None` for anything that isn't a
/// box-drawing/block character (which then renders through the font).
///
/// `scale` is the window's device scale factor. Every straight stroke is
/// snapped to the *device pixel* grid it implies — not for crispness alone,
/// but for continuity: a cell boundary at a fractional device pixel gets an
/// antialiasing ramp on both sides, and two abutting 50%-coverage edges
/// composite to 75% opacity, which perforated every multi-row `│` with a
/// lighter band at each row boundary. Snapped edges rasterize with no ramp at
/// all, so adjacent cells butt into one continuous solid — the same reason
/// kitty's cell-aligned box bitmaps tile seamlessly.
pub(crate) fn glyph(c: char, bounds: Bounds<Pixels>, scale: f32) -> Option<Vec<Ink>> {
    if !('\u{2500}'..='\u{259f}').contains(&c) {
        return None;
    }
    let g = Cell::new(&bounds, scale);
    if let Some((u, d, l, r)) = arms_of(c) {
        return Some(g.arms(u, d, l, r));
    }
    g.doubles(c)
        .or_else(|| g.rounded(c))
        .or_else(|| g.dashed(c))
        .or_else(|| g.diagonal(c))
        .or_else(|| g.blocks(c))
}

/// The weight of one arm (centre → edge) of a box character.
#[derive(Clone, Copy, PartialEq)]
enum Arm {
    None,
    Light,
    Heavy,
}

/// Cell geometry in f32, plus the light stroke thickness `t` (see
/// [`light_thickness`] for how that one is chosen).
struct Cell {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    cx: f32,
    cy: f32,
    t: f32,
    scale: f32,
}

/// The light stroke thickness for a cell `cell_width` wide, in logical pixels.
///
/// Two rules, in order:
///
/// 1. Derive from the cell *width* — a pure font-size proxy — never the height:
///    the height carries the line-height stretch, and a `─` that fattens when
///    the user opens up their line spacing would look broken.
/// 2. Then quantise so the result covers a whole number of device pixels.
///
/// Rule 2 keeps the nominal weight and the painted weight in agreement:
/// [`Cell::vstroke`] lays a stroke off in whole device pixels, and everything
/// positioned relative to `t` (the arm overshoot, the double-line separation,
/// `heavy = 2 × light`) should be reasoning about the same value the rasteriser
/// will actually produce.
///
/// Rounding the logical value *first* is what keeps 1x and 2x byte-identical to
/// what this module shipped with — those are the scales it was tuned and
/// visually verified at, so the fractional-scale fix must not disturb them.
fn light_thickness(cell_width: f32, scale: f32) -> f32 {
    let logical = (cell_width * 0.15).round().max(1.);
    (logical * scale).round().max(1.) / scale
}

impl Cell {
    fn new(b: &Bounds<Pixels>, scale: f32) -> Self {
        let x0 = b.origin.x.as_f32();
        let y0 = b.origin.y.as_f32();
        let x1 = x0 + b.size.width.as_f32();
        let y1 = y0 + b.size.height.as_f32();
        let scale = scale.max(0.1);
        Cell {
            x0,
            y0,
            x1,
            y1,
            cx: (x0 + x1) / 2.,
            cy: (y0 + y1) / 2.,
            t: light_thickness(x1 - x0, scale),
            scale,
        }
    }

    /// Snap a logical coordinate onto the device pixel grid.
    fn snap(&self, v: f32) -> f32 {
        (v * self.scale).round() / self.scale
    }

    /// A rectangle with every edge snapped to device pixels (see [`glyph`]).
    /// Snapping the two edges — not origin + size — is what keeps a shared
    /// cell boundary shared: both cells snap the same coordinate to the same
    /// pixel line, so consecutive `│` cells tile with zero gap and zero
    /// overlap whatever the window position.
    fn rectb(&self, x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        let (sx0, sy0) = (self.snap(x), self.snap(y));
        let (sx1, sy1) = (self.snap(x + w), self.snap(y + h));
        Bounds::new(point(px(sx0), px(sy0)), size(px(sx1 - sx0), px(sy1 - sy0)))
    }

    fn rect(&self, x: f32, y: f32, w: f32, h: f32) -> Ink {
        Ink::Rect(self.rectb(x, y, w, h))
    }

    /// A logical thickness as a whole number of device pixels, back in logical
    /// units. Never zero: a stroke that rounds away is worse than one that is
    /// a touch too thick.
    fn stroke_px(&self, w: f32) -> f32 {
        (w * self.scale).round().max(1.) / self.scale
    }

    /// A vertical stroke of logical width `w`, centred on `x`, spanning
    /// `ya..yb`.
    ///
    /// The two *ends* snap like any other edge, so a stroke that runs to a cell
    /// boundary still shares that boundary exactly with the cell beyond it —
    /// the tiling property [`rectb`](Self::rectb) exists for.
    ///
    /// The *width* is deliberately not a second pair of independent snaps. Two
    /// edges `w` apart land `w × scale` device pixels apart, and unless that is
    /// exactly a whole number the two `round`s straddle it — rounding apart in
    /// some cells and together in others, which made vertical rules alternate
    /// thin/thick across the columns of a TUI table at Windows' default 125% /
    /// 150% scaling. [`light_thickness`] picks `w` so the product is integral,
    /// but `f32` cannot always represent it exactly (a `1.5×` scale gives
    /// `2/1.5 × 1.5 = 2.0000001`), and a coordinate landing on a `.5` tie then
    /// rounds whichever way the error points. Laying the width off from the
    /// snapped near edge sidesteps the tie entirely: same weight everywhere,
    /// by construction rather than by luck.
    fn vstroke(&self, x: f32, w: f32, ya: f32, yb: f32) -> Ink {
        let (x0, y0, y1) = (self.snap(x - w / 2.), self.snap(ya), self.snap(yb));
        Ink::Rect(Bounds::new(
            point(px(x0), px(y0)),
            size(px(self.stroke_px(w)), px(y1 - y0)),
        ))
    }

    /// A horizontal stroke of logical width `w`, centred on `y`, spanning
    /// `xa..xb`. See [`vstroke`](Self::vstroke).
    fn hstroke(&self, y: f32, w: f32, xa: f32, xb: f32) -> Ink {
        let (y0, x0, x1) = (self.snap(y - w / 2.), self.snap(xa), self.snap(xb));
        Ink::Rect(Bounds::new(
            point(px(x0), px(y0)),
            size(px(x1 - x0), px(self.stroke_px(w))),
        ))
    }

    /// The light/heavy arm combinations: one rectangle per arm, each running
    /// from its cell edge to just past the centre.
    ///
    /// The overshoot (`m`, half the thickest arm) is what makes a corner: two
    /// perpendicular strokes that merely *meet* at the centre point leave a
    /// notch at the outside of the turn. Same-color opaque overlap costs
    /// nothing, so every arm overshoots by the same amount and any combination
    /// of weights joins solid.
    fn arms(&self, u: Arm, d: Arm, l: Arm, r: Arm) -> Vec<Ink> {
        let w = |a: Arm| match a {
            Arm::None => 0.,
            Arm::Light => self.t,
            Arm::Heavy => self.t * 2.,
        };
        let (wu, wd, wl, wr) = (w(u), w(d), w(l), w(r));
        let m = wu.max(wd).max(wl).max(wr) / 2.;
        let mut ink = Vec::new();
        if wu > 0. {
            ink.push(self.vstroke(self.cx, wu, self.y0, self.cy + m));
        }
        if wd > 0. {
            ink.push(self.vstroke(self.cx, wd, self.cy - m, self.y1));
        }
        if wl > 0. {
            ink.push(self.hstroke(self.cy, wl, self.x0, self.cx + m));
        }
        if wr > 0. {
            ink.push(self.hstroke(self.cy, wr, self.cx - m, self.x1));
        }
        ink
    }

    /// The double-line set (U+2550–U+256C), spelled out stroke by stroke.
    ///
    /// Doubles can't reuse the [`arms`](Self::arms) overshoot trick: their
    /// junctions are *open* — ╬ is four corner pieces around a hole, ╠'s inner
    /// stroke breaks where the branch leaves — so each character lists exactly
    /// the segments the Unicode chart draws, with endpoints snapped half a
    /// stroke past the line they join so corners close without crossing the
    /// gap.
    fn doubles(&self, c: char) -> Option<Vec<Ink>> {
        let t = self.t;
        let h = t / 2.;
        // The parallel strokes sit at centre ± d. At the 1px thickness of
        // ordinary font sizes this leaves a 3px gap — wide enough to survive
        // subpixel placement without the two strokes bleeding into one.
        let d = (t * 1.5).max(2.0);
        let (x0, x1, y0, y1, cx, cy) = (self.x0, self.x1, self.y0, self.y1, self.cx, self.cy);
        let (va, vb) = (cx - d, cx + d);
        let (ha, hb) = (cy - d, cy + d);
        let v = |x: f32, ya: f32, yb: f32| self.vstroke(x, t, ya, yb);
        let hz = |y: f32, xa: f32, xb: f32| self.hstroke(y, t, xa, xb);
        Some(match c {
            '═' => vec![hz(ha, x0, x1), hz(hb, x0, x1)],
            '║' => vec![v(va, y0, y1), v(vb, y0, y1)],
            '╒' => vec![hz(ha, cx - h, x1), hz(hb, cx - h, x1), v(cx, ha - h, y1)],
            '╓' => vec![hz(cy, va - h, x1), v(va, cy - h, y1), v(vb, cy - h, y1)],
            '╔' => vec![
                v(va, ha - h, y1),
                hz(ha, va - h, x1),
                v(vb, hb - h, y1),
                hz(hb, vb - h, x1),
            ],
            '╕' => vec![hz(ha, x0, cx + h), hz(hb, x0, cx + h), v(cx, ha - h, y1)],
            '╖' => vec![hz(cy, x0, vb + h), v(va, cy - h, y1), v(vb, cy - h, y1)],
            '╗' => vec![
                v(vb, ha - h, y1),
                hz(ha, x0, vb + h),
                v(va, hb - h, y1),
                hz(hb, x0, va + h),
            ],
            '╘' => vec![v(cx, y0, hb + h), hz(ha, cx - h, x1), hz(hb, cx - h, x1)],
            '╙' => vec![v(va, y0, cy + h), v(vb, y0, cy + h), hz(cy, va - h, x1)],
            '╚' => vec![
                v(va, y0, hb + h),
                hz(hb, va - h, x1),
                v(vb, y0, ha + h),
                hz(ha, vb - h, x1),
            ],
            '╛' => vec![v(cx, y0, hb + h), hz(ha, x0, cx + h), hz(hb, x0, cx + h)],
            '╜' => vec![v(va, y0, cy + h), v(vb, y0, cy + h), hz(cy, x0, vb + h)],
            '╝' => vec![
                v(vb, y0, hb + h),
                hz(hb, x0, vb + h),
                v(va, y0, ha + h),
                hz(ha, x0, va + h),
            ],
            '╞' => vec![v(cx, y0, y1), hz(ha, cx - h, x1), hz(hb, cx - h, x1)],
            '╟' => vec![v(va, y0, y1), v(vb, y0, y1), hz(cy, vb - h, x1)],
            '╠' => vec![
                v(va, y0, y1),
                v(vb, y0, ha + h),
                v(vb, hb - h, y1),
                hz(ha, vb - h, x1),
                hz(hb, vb - h, x1),
            ],
            '╡' => vec![v(cx, y0, y1), hz(ha, x0, cx + h), hz(hb, x0, cx + h)],
            '╢' => vec![v(va, y0, y1), v(vb, y0, y1), hz(cy, x0, va + h)],
            '╣' => vec![
                v(vb, y0, y1),
                v(va, y0, ha + h),
                v(va, hb - h, y1),
                hz(ha, x0, va + h),
                hz(hb, x0, va + h),
            ],
            '╤' => vec![hz(ha, x0, x1), hz(hb, x0, x1), v(cx, hb - h, y1)],
            '╥' => vec![hz(cy, x0, x1), v(va, cy - h, y1), v(vb, cy - h, y1)],
            '╦' => vec![
                hz(ha, x0, x1),
                hz(hb, x0, va + h),
                hz(hb, vb - h, x1),
                v(va, hb - h, y1),
                v(vb, hb - h, y1),
            ],
            '╧' => vec![hz(ha, x0, x1), hz(hb, x0, x1), v(cx, y0, ha + h)],
            '╨' => vec![hz(cy, x0, x1), v(va, y0, cy + h), v(vb, y0, cy + h)],
            '╩' => vec![
                hz(hb, x0, x1),
                hz(ha, x0, va + h),
                hz(ha, vb - h, x1),
                v(va, y0, ha + h),
                v(vb, y0, ha + h),
            ],
            '╪' => vec![v(cx, y0, y1), hz(ha, x0, x1), hz(hb, x0, x1)],
            '╫' => vec![v(va, y0, y1), v(vb, y0, y1), hz(cy, x0, x1)],
            '╬' => vec![
                v(va, y0, ha + h),
                v(vb, y0, ha + h),
                v(va, hb - h, y1),
                v(vb, hb - h, y1),
                hz(ha, x0, va + h),
                hz(ha, vb - h, x1),
                hz(hb, x0, va + h),
                hz(hb, vb - h, x1),
            ],
            _ => return None,
        })
    }

    /// The rounded corners ╭ ╮ ╯ ╰ — two straight stubs to the cell edges plus
    /// a quarter-circle band between them. `sx`/`sy` name the quadrant the arms
    /// leave through: ╭ runs down (+1) and right (+1).
    ///
    /// The band is a fan of small convex quads, one per arc step, NOT a single
    /// outer-arc/inner-arc outline. That outline is concave, and gpui fills a
    /// path as a triangle fan from its first vertex — a concave contour gets
    /// its whole hollow covered, which rendered every corner as a solid
    /// quarter-disc blob the first time around. Each quad is convex, so each
    /// fills exactly itself, and at stroke widths of a few pixels twelve steps
    /// are indistinguishable from a true arc.
    fn rounded(&self, c: char) -> Option<Vec<Ink>> {
        let (sx, sy): (f32, f32) = match c {
            '╭' => (1., 1.),
            '╮' => (-1., 1.),
            '╯' => (-1., -1.),
            '╰' => (1., -1.),
            _ => return None,
        };
        let h = self.t / 2.;
        // The largest radius that keeps the arc inside the cell on its short
        // axis; the straight stubs cover whatever the long axis has left over.
        let r = ((self.x1 - self.x0).min(self.y1 - self.y0) / 2.).max(h * 2.);
        let (cx, cy) = (self.cx, self.cy);
        let mut ink = Vec::new();
        // Straight stubs from the arc's ends to the cell edges (zero-length
        // when the radius already spans the half-axis). Each stub reaches one
        // device pixel *into* the arc band: the stub is pixel-snapped, the arc
        // isn't, and without the overlap that mismatch reopens a hairline
        // seam exactly where they hand off.
        let lap = 1. / self.scale;
        if sy > 0. {
            ink.push(self.vstroke(cx, self.t, cy + r - lap, self.y1));
        } else {
            ink.push(self.vstroke(cx, self.t, self.y0, cy - r + lap));
        }
        if sx > 0. {
            ink.push(self.hstroke(cy, self.t, cx + r - lap, self.x1));
        } else {
            ink.push(self.hstroke(cy, self.t, self.x0, cx - r + lap));
        }
        // The arc band, from the vertical stub (θ=0) to the horizontal one
        // (θ=π/2) around the arc centre one radius into the quadrant.
        //
        // How this renders decides whether the corner looks like kitty's or
        // not, and gpui's pipeline dictates the shape (learned the hard way,
        // twice):
        //
        // * A path contour is filled as a triangle FAN from its start vertex,
        //   and coverage in the intermediate texture only accumulates — there
        //   is no winding cancellation. A whole-band outline is concave, so
        //   its fan covered the hollow and every corner rendered as a solid
        //   quarter-disc blob. Each contour must therefore be *star-shaped
        //   from its start vertex*: 30° slices of a thin band are, a 90° band
        //   is not.
        // * All contours ride in ONE Path. Paths composite as premultiplied
        //   sprites, so two separately painted segments overlap their
        //   antialiased edges at 75% opacity — the seam at every joint of the
        //   first polyline attempt. Within a single path the 4x-MSAA samples
        //   partition cleanly across shared edges instead.
        // * The outer edge is a real quadratic (`curve_to`), which the shader
        //   antialiases *analytically* (Loop–Blinn signed distance) — the
        //   smooth continuous ramp kitty gets from supersampling. The inner
        //   edge can't be a curve: with no winding, a concave-side bulge can
        //   only over-cover. It is a fine polyline instead, whose chord error
        //   at 7.5° steps (< 0.1px at cell sizes) hides inside the MSAA.
        let (ax, ay) = (cx + sx * r, cy + sy * r);
        let at = |radius: f32, theta: f32| {
            let (x, y) = (
                ax - sx * radius * theta.cos(),
                ay - sy * radius * theta.sin(),
            );
            point(px(x), px(y))
        };
        const SEGS: usize = 3;
        const INNER_PTS: usize = 4;
        let step = std::f32::consts::FRAC_PI_2 / SEGS as f32;
        let mut path: Option<gpui::Path<Pixels>> = None;
        for i in 0..SEGS {
            let t0 = step * i as f32;
            let t1 = step * (i + 1) as f32;
            let start = at(r + h, t0);
            let p = match path.as_mut() {
                Some(p) => {
                    p.move_to(start);
                    p
                }
                None => path.insert(gpui::Path::new(start)),
            };
            // Control point at the tangents' intersection: the exact
            // quadratic through both endpoints for this arc slice.
            let ctrl = at((r + h) / (step / 2.).cos(), (t0 + t1) / 2.);
            p.curve_to(at(r + h, t1), ctrl);
            p.line_to(at(r - h, t1));
            for k in (0..INNER_PTS).rev() {
                p.line_to(at(r - h, t0 + (t1 - t0) * k as f32 / INNER_PTS as f32));
            }
        }
        if let Some(p) = path {
            ink.push(Ink::Path(p));
        }
        Some(ink)
    }

    /// The dashed lines: n dashes, each 70% of its slot, centred. Deliberately
    /// *not* edge-to-edge — a dashed line is supposed to read as broken, and
    /// this matches how the font glyphs space them.
    fn dashed(&self, c: char) -> Option<Vec<Ink>> {
        let (n, heavy, vertical) = match c {
            '╌' => (2, false, false),
            '╍' => (2, true, false),
            '╎' => (2, false, true),
            '╏' => (2, true, true),
            '┄' => (3, false, false),
            '┅' => (3, true, false),
            '┆' => (3, false, true),
            '┇' => (3, true, true),
            '┈' => (4, false, false),
            '┉' => (4, true, false),
            '┊' => (4, false, true),
            '┋' => (4, true, true),
            _ => return None,
        };
        let w = if heavy { self.t * 2. } else { self.t };
        let (a0, a1) = if vertical {
            (self.y0, self.y1)
        } else {
            (self.x0, self.x1)
        };
        let seg = (a1 - a0) / n as f32;
        let ink = (0..n)
            .map(|i| {
                let s = a0 + seg * (i as f32 + 0.15);
                let len = seg * 0.7;
                if vertical {
                    self.vstroke(self.cx, w, s, s + len)
                } else {
                    self.hstroke(self.cy, w, s, s + len)
                }
            })
            .collect();
        Some(ink)
    }

    /// The diagonals ╱ ╲ ╳ as corner-to-corner parallelograms. The offset is
    /// vertical (not perpendicular) so every vertex stays inside the cell; its
    /// length is scaled so the *perpendicular* stroke width still comes out at
    /// the light thickness.
    fn diagonal(&self, c: char) -> Option<Vec<Ink>> {
        let (w, hgt) = (self.x1 - self.x0, self.y1 - self.y0);
        let v = self.t * (w * w + hgt * hgt).sqrt() / w;
        let p = |x: f32, y: f32| point(px(x), px(y));
        let quad = |top_x: f32, bot_x: f32| {
            let mut path = gpui::Path::new(p(top_x, self.y0));
            path.line_to(p(top_x, self.y0 + v));
            path.line_to(p(bot_x, self.y1));
            path.line_to(p(bot_x, self.y1 - v));
            Ink::Path(path)
        };
        Some(match c {
            '╱' => vec![quad(self.x1, self.x0)],
            '╲' => vec![quad(self.x0, self.x1)],
            '╳' => vec![quad(self.x1, self.x0), quad(self.x0, self.x1)],
            _ => return None,
        })
    }

    /// The block elements U+2580–U+259F: eighths, halves, quadrants, and the
    /// ░▒▓ shades (a full-cell wash at a quarter / half / three quarters of the
    /// foreground's alpha).
    fn blocks(&self, c: char) -> Option<Vec<Ink>> {
        let (x0, x1, y0, y1, cx, cy) = (self.x0, self.x1, self.y0, self.y1, self.cx, self.cy);
        let (w, hgt) = (x1 - x0, y1 - y0);
        let r = |x: f32, y: f32, ww: f32, hh: f32| self.rect(x, y, ww, hh);
        let ul = || r(x0, y0, cx - x0, cy - y0);
        let ur = || r(cx, y0, x1 - cx, cy - y0);
        let ll = || r(x0, cy, cx - x0, y1 - cy);
        let lr = || r(cx, cy, x1 - cx, y1 - cy);
        Some(match c {
            '▀' => vec![r(x0, y0, w, hgt / 2.)],
            // ▁ (1/8) through █ (the full block): lower k eighths.
            '▁'..='█' => {
                let k = (c as u32 - 0x2580) as f32;
                let hh = hgt * k / 8.;
                vec![r(x0, y1 - hh, w, hh)]
            }
            // ▉ (7/8) through ▏ (1/8): left k eighths.
            '▉'..='▏' => {
                let k = (0x2590 - c as u32) as f32;
                vec![r(x0, y0, w * k / 8., hgt)]
            }
            '▐' => vec![r(cx, y0, x1 - cx, hgt)],
            '░' => vec![Ink::Shade(self.rectb(x0, y0, w, hgt), 0.25)],
            '▒' => vec![Ink::Shade(self.rectb(x0, y0, w, hgt), 0.5)],
            '▓' => vec![Ink::Shade(self.rectb(x0, y0, w, hgt), 0.75)],
            '▔' => vec![r(x0, y0, w, hgt / 8.)],
            '▕' => vec![r(x1 - w / 8., y0, w / 8., hgt)],
            '▖' => vec![ll()],
            '▗' => vec![lr()],
            '▘' => vec![ul()],
            '▙' => vec![ul(), ll(), lr()],
            '▚' => vec![ul(), lr()],
            '▛' => vec![ul(), ur(), ll()],
            '▜' => vec![ul(), ur(), lr()],
            '▝' => vec![ur()],
            '▞' => vec![ur(), ll()],
            '▟' => vec![ur(), ll(), lr()],
            _ => return None,
        })
    }
}

/// Decode the light/heavy arm combinations: the solid lines, corners, tees and
/// crosses of U+2500–U+254B, and the half/mixed lines of U+2574–U+257F. Order
/// is (up, down, left, right).
fn arms_of(c: char) -> Option<(Arm, Arm, Arm, Arm)> {
    use Arm::{Heavy as H, Light as L, None as N};
    Some(match c {
        '─' => (N, N, L, L),
        '━' => (N, N, H, H),
        '│' => (L, L, N, N),
        '┃' => (H, H, N, N),
        '┌' => (N, L, N, L),
        '┍' => (N, L, N, H),
        '┎' => (N, H, N, L),
        '┏' => (N, H, N, H),
        '┐' => (N, L, L, N),
        '┑' => (N, L, H, N),
        '┒' => (N, H, L, N),
        '┓' => (N, H, H, N),
        '└' => (L, N, N, L),
        '┕' => (L, N, N, H),
        '┖' => (H, N, N, L),
        '┗' => (H, N, N, H),
        '┘' => (L, N, L, N),
        '┙' => (L, N, H, N),
        '┚' => (H, N, L, N),
        '┛' => (H, N, H, N),
        '├' => (L, L, N, L),
        '┝' => (L, L, N, H),
        '┞' => (H, L, N, L),
        '┟' => (L, H, N, L),
        '┠' => (H, H, N, L),
        '┡' => (H, L, N, H),
        '┢' => (L, H, N, H),
        '┣' => (H, H, N, H),
        '┤' => (L, L, L, N),
        '┥' => (L, L, H, N),
        '┦' => (H, L, L, N),
        '┧' => (L, H, L, N),
        '┨' => (H, H, L, N),
        '┩' => (H, L, H, N),
        '┪' => (L, H, H, N),
        '┫' => (H, H, H, N),
        '┬' => (N, L, L, L),
        '┭' => (N, L, H, L),
        '┮' => (N, L, L, H),
        '┯' => (N, L, H, H),
        '┰' => (N, H, L, L),
        '┱' => (N, H, H, L),
        '┲' => (N, H, L, H),
        '┳' => (N, H, H, H),
        '┴' => (L, N, L, L),
        '┵' => (L, N, H, L),
        '┶' => (L, N, L, H),
        '┷' => (L, N, H, H),
        '┸' => (H, N, L, L),
        '┹' => (H, N, H, L),
        '┺' => (H, N, L, H),
        '┻' => (H, N, H, H),
        '┼' => (L, L, L, L),
        '┽' => (L, L, H, L),
        '┾' => (L, L, L, H),
        '┿' => (L, L, H, H),
        '╀' => (H, L, L, L),
        '╁' => (L, H, L, L),
        '╂' => (H, H, L, L),
        '╃' => (H, L, H, L),
        '╄' => (H, L, L, H),
        '╅' => (L, H, H, L),
        '╆' => (L, H, L, H),
        '╇' => (H, L, H, H),
        '╈' => (L, H, H, H),
        '╉' => (H, H, H, L),
        '╊' => (H, H, L, H),
        '╋' => (H, H, H, H),
        '╴' => (N, N, L, N),
        '╵' => (L, N, N, N),
        '╶' => (N, N, N, L),
        '╷' => (N, L, N, N),
        '╸' => (N, N, H, N),
        '╹' => (H, N, N, N),
        '╺' => (N, N, N, H),
        '╻' => (N, H, N, N),
        '╼' => (N, N, L, H),
        '╽' => (L, H, N, N),
        '╾' => (N, N, H, L),
        '╿' => (H, L, N, N),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cell with the proportions the bug shipped in: a 15px font's ~9px
    /// advance stretched to a 21px line by `line_height: 1.4`.
    fn cell() -> Bounds<Pixels> {
        Bounds::new(point(px(10.), px(20.)), size(px(9.), px(21.)))
    }

    /// min_x / max_x / min_y / max_y over every rect corner and path vertex.
    fn extents(ink: &[Ink]) -> (f32, f32, f32, f32) {
        let (mut nx, mut xx, mut ny, mut xy) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
        let mut visit = |x: f32, y: f32| {
            nx = nx.min(x);
            xx = xx.max(x);
            ny = ny.min(y);
            xy = xy.max(y);
        };
        for i in ink {
            match i {
                Ink::Rect(b) | Ink::Shade(b, _) => {
                    let (x, y) = (b.origin.x.as_f32(), b.origin.y.as_f32());
                    visit(x, y);
                    visit(x + b.size.width.as_f32(), y + b.size.height.as_f32());
                }
                Ink::Path(p) => {
                    for v in &p.vertices {
                        visit(v.xy_position.x.as_f32(), v.xy_position.y.as_f32());
                    }
                }
            }
        }
        (nx, xx, ny, xy)
    }

    /// Every character in U+2500–U+259F must decode to native ink — one that
    /// silently falls through to the font reintroduces the row-boundary gap
    /// for exactly that character, which is worse than uniform behavior in
    /// either direction.
    #[test]
    fn the_whole_range_is_covered() {
        for cp in 0x2500u32..=0x259f {
            let c = char::from_u32(cp).unwrap();
            assert!(
                glyph(c, cell(), 1.).is_some(),
                "U+{cp:04X} {c} fell through to the font"
            );
        }
    }

    /// Nothing may paint outside its own cell: box characters tile, and one
    /// cell's overshoot is its neighbor's artifact.
    ///
    /// The tolerance is half a pixel, not exact: a quadratic's *control point*
    /// sits slightly outside the ink it bounds (tangent-intersection, ~3.5%
    /// past the arc radius), and `extents` reads raw vertices. The curve
    /// itself never leaves the cell.
    #[test]
    fn ink_stays_inside_the_cell() {
        let b = cell();
        let (x0, y0) = (b.origin.x.as_f32(), b.origin.y.as_f32());
        let (x1, y1) = (x0 + b.size.width.as_f32(), y0 + b.size.height.as_f32());
        for cp in 0x2500u32..=0x259f {
            let c = char::from_u32(cp).unwrap();
            let (nx, xx, ny, xy) = extents(&glyph(c, b, 1.).unwrap());
            assert!(
                nx >= x0 - 0.5 && xx <= x1 + 0.5 && ny >= y0 - 0.5 && xy <= y1 + 0.5,
                "U+{cp:04X} {c} paints outside the cell: \
                 x {nx}..{xx} vs {x0}..{x1}, y {ny}..{xy} vs {y0}..{y1}"
            );
        }
    }

    /// The regression this module exists for: every arm must reach its cell
    /// edge *exactly*, so vertical runs connect across the line-height gap and
    /// horizontal runs connect across cells. Checked for the whole arms table
    /// — including the mixed and half lines — not just `│`.
    #[test]
    fn arms_reach_their_edges() {
        let b = cell();
        let (x0, y0) = (b.origin.x.as_f32(), b.origin.y.as_f32());
        let (x1, y1) = (x0 + b.size.width.as_f32(), y0 + b.size.height.as_f32());
        for cp in 0x2500u32..=0x259f {
            let c = char::from_u32(cp).unwrap();
            let Some((u, d, l, r)) = arms_of(c) else {
                continue;
            };
            let (nx, xx, ny, xy) = extents(&glyph(c, b, 1.).unwrap());
            if u != Arm::None {
                assert_eq!(ny, y0, "{c}: up arm misses the top edge");
            }
            if d != Arm::None {
                assert_eq!(xy, y1, "{c}: down arm misses the bottom edge");
            }
            if l != Arm::None {
                assert_eq!(nx, x0, "{c}: left arm misses the left edge");
            }
            if r != Arm::None {
                assert_eq!(xx, x1, "{c}: right arm misses the right edge");
            }
        }
    }

    /// Same edge guarantee for the shapes that aren't plain arms: the doubles,
    /// the rounded corners, and the diagonals all tile too.
    #[test]
    fn doubles_rounded_and_diagonals_reach_their_edges() {
        let b = cell();
        let (x0, y0) = (b.origin.x.as_f32(), b.origin.y.as_f32());
        let (x1, y1) = (x0 + b.size.width.as_f32(), y0 + b.size.height.as_f32());
        // (char, up, down, left, right)
        let expect = [
            ('═', false, false, true, true),
            ('║', true, true, false, false),
            ('╔', false, true, false, true),
            ('╬', true, true, true, true),
            ('╠', true, true, false, true),
            ('╦', false, true, true, true),
            ('╭', false, true, false, true),
            ('╮', false, true, true, false),
            ('╯', true, false, true, false),
            ('╰', true, false, false, true),
            ('╱', true, true, true, true),
            ('╲', true, true, true, true),
        ];
        for (c, u, d, l, r) in expect {
            let (nx, xx, ny, xy) = extents(&glyph(c, b, 1.).unwrap());
            if u {
                assert_eq!(ny, y0, "{c}: misses the top edge");
            }
            if d {
                assert_eq!(xy, y1, "{c}: misses the bottom edge");
            }
            if l {
                assert_eq!(nx, x0, "{c}: misses the left edge");
            }
            if r {
                assert_eq!(xx, x1, "{c}: misses the right edge");
            }
        }
    }

    /// ╬ is four corner pieces around an open centre — the one double junction
    /// where "just extend everything through the middle" would visibly lie.
    #[test]
    fn double_cross_keeps_its_open_centre() {
        let b = cell();
        let cx = b.origin.x.as_f32() + b.size.width.as_f32() / 2.;
        let cy = b.origin.y.as_f32() + b.size.height.as_f32() / 2.;
        for i in glyph('╬', b, 1.).unwrap() {
            let Ink::Rect(r) = i else {
                panic!("╬ should be rects only");
            };
            let (x, y) = (r.origin.x.as_f32(), r.origin.y.as_f32());
            let inside = cx > x
                && cx < x + r.size.width.as_f32()
                && cy > y
                && cy < y + r.size.height.as_f32();
            assert!(!inside, "╬'s centre is covered");
        }
    }

    /// Blocks: the full block is the full cell, the halves are exact halves,
    /// and the shades wash the whole cell at their nominal alpha.
    #[test]
    fn blocks_cover_their_nominal_area() {
        let b = cell();
        let (x0, y0) = (b.origin.x.as_f32(), b.origin.y.as_f32());
        let (w, h) = (b.size.width.as_f32(), b.size.height.as_f32());
        let (nx, xx, ny, xy) = extents(&glyph('█', b, 1.).unwrap());
        assert_eq!(
            (nx, xx, ny, xy),
            (x0, x0 + w, y0, y0 + h),
            "█ isn't the full cell"
        );
        // Interior edges (the half-cell split) may sit up to half a device
        // pixel from nominal after snapping; the outer edges stay exact.
        let (_, _, ny, xy) = extents(&glyph('▀', b, 1.).unwrap());
        assert_eq!(ny, y0, "▀ doesn't reach the top");
        assert!((xy - (y0 + h / 2.)).abs() <= 0.5, "▀ isn't the top half");
        let (_, _, ny, xy) = extents(&glyph('▄', b, 1.).unwrap());
        assert_eq!(xy, y0 + h, "▄ doesn't reach the bottom");
        assert!((ny - (y0 + h / 2.)).abs() <= 0.5, "▄ isn't the bottom half");
        for (c, alpha) in [('░', 0.25), ('▒', 0.5), ('▓', 0.75)] {
            let ink = glyph(c, b, 1.).unwrap();
            assert_eq!(ink.len(), 1);
            let Ink::Shade(r, a) = &ink[0] else {
                panic!("{c} should be a shade");
            };
            assert_eq!(*a, alpha);
            assert_eq!(r.size.width.as_f32(), w, "{c} doesn't wash the full cell");
        }
    }

    /// Heavy strokes must actually be heavier than light ones, and a light
    /// stroke never vanishes (≥ 1px) however small the cell.
    #[test]
    fn stroke_weights_are_ordered_and_visible() {
        let light = {
            let Ink::Rect(r) = &glyph('│', cell(), 1.).unwrap()[0] else {
                panic!()
            };
            r.size.width.as_f32()
        };
        let heavy = {
            let Ink::Rect(r) = &glyph('┃', cell(), 1.).unwrap()[0] else {
                panic!()
            };
            r.size.width.as_f32()
        };
        assert!(light >= 1., "light stroke thinner than a pixel");
        assert!(heavy > light, "heavy stroke isn't heavier");
        // A pathologically narrow cell still yields visible ink.
        let tiny = Bounds::new(point(px(0.), px(0.)), size(px(2.), px(4.)));
        let Ink::Rect(r) = &glyph('│', tiny, 1.).unwrap()[0] else {
            panic!()
        };
        assert!(r.size.width.as_f32() >= 1.);
    }

    /// The seam regression: with the window at a fractional device-pixel
    /// offset, every straight stroke must still land on whole device pixels.
    /// An unsnapped edge rasterizes an antialiasing ramp, and two abutting
    /// ramps composite to 75% opacity — the perforated `│` runs this module
    /// was reported for a second time over.
    #[test]
    fn straight_strokes_snap_to_device_pixels() {
        let scale = 2.0;
        // Deliberately misaligned: fractional origin and cell width.
        let b = Bounds::new(point(px(10.37), px(20.11)), size(px(9.03), px(21.)));
        let on_grid = |v: f32| ((v * scale).round() - v * scale).abs() < 1e-3;
        for cp in 0x2500u32..=0x259f {
            let c = char::from_u32(cp).unwrap();
            for i in glyph(c, b, scale).unwrap() {
                let (Ink::Rect(r) | Ink::Shade(r, _)) = i else {
                    continue; // arcs and diagonals antialias on purpose
                };
                let (x, y) = (r.origin.x.as_f32(), r.origin.y.as_f32());
                let (x2, y2) = (x + r.size.width.as_f32(), y + r.size.height.as_f32());
                assert!(
                    on_grid(x) && on_grid(y) && on_grid(x2) && on_grid(y2),
                    "U+{cp:04X} {c}: stroke edge off the device grid \
                     ({x}, {y})..({x2}, {y2}) at scale {scale}"
                );
            }
        }
        // And two vertically adjacent `│` cells must share their boundary
        // exactly — same coordinate in, same snapped pixel line out.
        let below = Bounds::new(point(px(10.37), px(41.11)), size(px(9.03), px(21.)));
        let bottom = extents(&glyph('│', b, scale).unwrap()).3;
        let top = extents(&glyph('│', below, scale).unwrap()).2;
        assert_eq!(bottom, top, "adjacent │ cells no longer tile");
    }

    /// Every column must draw `│` at the *same* weight, and every row must draw
    /// `─` at the same weight, at any scale factor — not just the integer ones.
    ///
    /// Note what the test above does *not* catch: it asserts each edge lands on
    /// the device grid, which a 1-device-pixel stroke and a 2-device-pixel
    /// stroke both satisfy. Windows' default 125%/150% display scaling put a
    /// 1-logical-pixel stroke a non-integer number of device pixels wide, and
    /// the two independent edge snaps then rounded apart in some columns and
    /// together in others: vertical rules alternated thin/thick across a TUI
    /// table, horizontal rules alternated down it. Both 1x and 2x are blind to
    /// it by construction, so the earlier fixtures could never have failed.
    #[test]
    fn stroke_weight_is_uniform_across_cells_at_any_scale() {
        // Realistic cell metrics: a 13/15/16px font's advance, line_height 1.4.
        for (cw, lh) in [(7.8f32, 18.0f32), (9.03, 21.0), (9.6, 22.0), (10.8, 25.0)] {
            for scale in [1.0f32, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0] {
                let widths: Vec<f32> = (0..24)
                    .map(|i| {
                        let b = Bounds::new(point(px(cw * i as f32), px(0.)), size(px(cw), px(lh)));
                        let Ink::Rect(r) = &glyph('│', b, scale).unwrap()[0] else {
                            panic!("│ should be a rect")
                        };
                        (r.size.width.as_f32() * scale).round()
                    })
                    .collect();
                let (lo, hi) = (
                    widths.iter().cloned().fold(f32::MAX, f32::min),
                    widths.iter().cloned().fold(f32::MIN, f32::max),
                );
                assert_eq!(
                    lo, hi,
                    "│ weight varies {lo}..{hi} device px across columns \
                     (cell_width {cw}, scale {scale}): {widths:?}"
                );
                assert!(lo >= 1., "│ thinner than a device pixel at scale {scale}");

                let heights: Vec<f32> = (0..24)
                    .map(|r| {
                        let b = Bounds::new(point(px(0.), px(lh * r as f32)), size(px(cw), px(lh)));
                        let Ink::Rect(rect) = &glyph('─', b, scale).unwrap()[0] else {
                            panic!("─ should be a rect")
                        };
                        (rect.size.height.as_f32() * scale).round()
                    })
                    .collect();
                let (lo, hi) = (
                    heights.iter().cloned().fold(f32::MAX, f32::min),
                    heights.iter().cloned().fold(f32::MIN, f32::max),
                );
                assert_eq!(
                    lo, hi,
                    "─ weight varies {lo}..{hi} device px across rows \
                     (cell_width {cw}, scale {scale}): {heights:?}"
                );
            }
        }
    }

    /// Quantising the thickness in device space must not change what 1x and 2x
    /// already rendered — those are the two scales the module was tuned and
    /// visually verified at.
    #[test]
    fn integer_scales_keep_their_previous_thickness() {
        for (cw, lh) in [(7.8f32, 18.0f32), (9.03, 21.0), (9.6, 22.0), (10.8, 25.0)] {
            for scale in [1.0f32, 2.0, 3.0] {
                let previous = (cw * 0.15).round().max(1.);
                assert_eq!(
                    light_thickness(cw, scale),
                    previous,
                    "cell_width {cw} at scale {scale} changed weight"
                );
                // And heavy stays exactly twice light, as `arms` assumes.
                let b = Bounds::new(point(px(0.), px(0.)), size(px(cw), px(lh)));
                let Ink::Rect(l) = &glyph('│', b, scale).unwrap()[0] else {
                    panic!()
                };
                let Ink::Rect(h) = &glyph('┃', b, scale).unwrap()[0] else {
                    panic!()
                };
                assert!(h.size.width.as_f32() > l.size.width.as_f32());
            }
        }
    }
}
