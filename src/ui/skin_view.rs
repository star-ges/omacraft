//! A rotatable 3D player model, without a 3D renderer. Every skin texel
//! becomes a small quad on its box face; each frame they're rotated,
//! projected orthographically and painted back to front as one mesh. One
//! quad per texel keeps the pixels crisp, lets transparent texels of the
//! outer layer simply drop out, and sorts far better than whole faces would.

use egui::{Color32, Mesh, Painter, Pos2, Rect, Sense, Ui, vec2};
use image::RgbaImage;

use crate::theme::Theme;

type V3 = [f32; 3];

struct Quad {
    corners: [V3; 4],
    normal: V3,
    color: [u8; 4],
}

pub struct Model {
    quads: Vec<Quad>,
}

/// One box of the model, in skin pixels. `min` is in model space: x to the
/// viewer's right, y up from the feet, z towards the viewer.
struct Part {
    uv: (u32, u32),
    size: (u32, u32, u32),
    min: V3,
    inflate: f32,
    /// Outer layer: transparent texels drop out. The base layer is opaque.
    outer: bool,
    /// Legacy skins draw the left limbs as mirrored right ones.
    mirror: bool,
}

const fn part(uv: (u32, u32), size: (u32, u32, u32), min: V3, inflate: f32, outer: bool) -> Part {
    Part { uv, size, min, inflate, outer, mirror: false }
}

impl Model {
    pub fn build(skin: &RgbaImage, slim: bool, cape: Option<&RgbaImage>) -> Model {
        let legacy = skin.height() * 2 == skin.width();
        let aw = if slim { 3 } else { 4 };
        let ax = if slim { -7.0 } else { -8.0 };
        let mut parts = vec![
            part((0, 0), (8, 8, 8), [-4.0, 24.0, -4.0], 0.0, false),
            part((16, 16), (8, 12, 4), [-4.0, 12.0, -2.0], 0.0, false),
            part((40, 16), (aw, 12, 4), [ax, 12.0, -2.0], 0.0, false),
            part((0, 16), (4, 12, 4), [-4.0, 0.0, -2.0], 0.0, false),
            part((32, 0), (8, 8, 8), [-4.0, 24.0, -4.0], 0.5, true),
        ];
        if legacy {
            // The right arm and leg again, mirrored onto the left.
            parts.push(Part { mirror: true, ..part((40, 16), (aw, 12, 4), [ax, 12.0, -2.0], 0.0, false) });
            parts.push(Part { mirror: true, ..part((0, 16), (4, 12, 4), [-4.0, 0.0, -2.0], 0.0, false) });
        } else {
            parts.extend([
                part((32, 48), (aw, 12, 4), [4.0, 12.0, -2.0], 0.0, false),
                part((16, 48), (4, 12, 4), [0.0, 0.0, -2.0], 0.0, false),
                part((16, 32), (8, 12, 4), [-4.0, 12.0, -2.0], 0.25, true),
                part((40, 32), (aw, 12, 4), [ax, 12.0, -2.0], 0.25, true),
                part((48, 48), (aw, 12, 4), [4.0, 12.0, -2.0], 0.25, true),
                part((0, 32), (4, 12, 4), [-4.0, 0.0, -2.0], 0.25, true),
                part((0, 48), (4, 12, 4), [0.0, 0.0, -2.0], 0.25, true),
            ]);
        }
        // Vanilla's fix for old skins whose hat layer was painted solid:
        // if it has no transparent texel at all, it isn't drawn.
        let hide_hat = legacy && (32..64).all(|x| (0..16).all(|y| texel(skin, x, y)[3] >= 128));

        let mut quads = Vec::new();
        for p in &parts {
            if hide_hat && p.uv == (32, 0) {
                continue;
            }
            let start = quads.len();
            add_box(&mut quads, skin, p);
            if p.mirror {
                for q in &mut quads[start..] {
                    for c in &mut q.corners {
                        c[0] = -c[0];
                    }
                    q.normal[0] = -q.normal[0];
                }
            }
        }

        // The cape hangs from the shoulders, its outside facing backwards,
        // swung out a little at the bottom.
        if let Some(cape) = cape {
            let start = quads.len();
            add_box(&mut quads, cape, &part((0, 0), (10, 16, 1), [-5.0, 8.0, 2.0], 0.0, false));
            let (s, c) = 10f32.to_radians().sin_cos();
            for q in &mut quads[start..] {
                for v in q.corners.iter_mut().chain(std::iter::once(&mut q.normal)) {
                    // Turn it around (180° about y)…
                    v[0] = -v[0];
                    v[2] = -v[2];
                }
                for v in &mut q.corners {
                    // …and tilt it about the top edge (y 24, z -2).
                    let (y, z) = (v[1] - 24.0, v[2] + 2.0);
                    v[1] = 24.0 + y * c - z * s;
                    v[2] = -2.0 + y * s + z * c;
                }
                let n = q.normal;
                q.normal = [n[0], n[1] * c - n[2] * s, n[1] * s + n[2] * c];
            }
        }
        Model { quads }
    }

    /// Paint into `rect`, turned `yaw` about the vertical axis and tipped
    /// `pitch` towards the viewer (radians).
    pub fn paint(&self, painter: &Painter, rect: Rect, yaw: f32, pitch: f32) {
        let (sy, cy) = yaw.sin_cos();
        let (sp, cp) = pitch.sin_cos();
        // About the model's middle, so it turns in place.
        let rot = |v: V3, centre: f32| {
            let (x, y, z) = (v[0], v[1] - centre, v[2]);
            let (x, z) = (x * cy + z * sy, -x * sy + z * cy);
            let (y, z) = (y * cp - z * sp, y * sp + z * cp);
            [x, y, z]
        };
        let scale = (rect.height() / 40.0).min(rect.width() / 26.0);
        let mid = rect.center();
        let to_screen = |v: V3| Pos2::new(mid.x + v[0] * scale, mid.y - v[1] * scale);
        let light = norm([-0.4, 0.5, 0.8]);

        let mut visible: Vec<(f32, [Pos2; 4], Color32)> = Vec::with_capacity(self.quads.len() / 2);
        for q in &self.quads {
            let n = rot(q.normal, 0.0);
            if n[2] <= 1e-4 {
                continue;
            }
            let c = q.corners.map(|v| rot(v, 16.0));
            let depth = c.iter().map(|v| v[2]).sum::<f32>() / 4.0;
            let shade = 0.62 + 0.38 * dot(n, light).max(0.0);
            let [r, g, b, a] = q.color;
            let f = |x: u8| (x as f32 * shade).round() as u8;
            visible.push((depth, c.map(to_screen), Color32::from_rgba_unmultiplied(f(r), f(g), f(b), a)));
        }
        visible.sort_by(|a, b| a.0.total_cmp(&b.0));

        let mut mesh = Mesh::default();
        for (_, pts, color) in visible {
            let i = mesh.vertices.len() as u32;
            for p in pts {
                mesh.colored_vertex(p, color);
            }
            mesh.add_triangle(i, i + 1, i + 2);
            mesh.add_triangle(i, i + 2, i + 3);
        }
        painter.with_clip_rect(rect).add(mesh);
    }
}

/// Reads a texel of a 64-wide layout, scaled for larger textures.
fn texel(img: &RgbaImage, x: u32, y: u32) -> [u8; 4] {
    let s = (img.width() / 64).max(1);
    let (x, y) = (x * s, y * s);
    if x < img.width() && y < img.height() { img.get_pixel(x, y).0 } else { [0; 4] }
}

/// The six faces of a box, as the standard Minecraft box UV layout puts them:
/// a top strip of [top][bottom] and below it [right][front][left][back].
fn add_box(out: &mut Vec<Quad>, img: &RgbaImage, p: &Part) {
    let (u, v) = p.uv;
    let (w, h, d) = p.size;
    let i = p.inflate;
    let (x0, x1) = (p.min[0] - i, p.min[0] + w as f32 + i);
    let (y0, y1) = (p.min[1] - i, p.min[1] + h as f32 + i);
    let (z0, z1) = (p.min[2] - i, p.min[2] + d as f32 + i);
    let (sx, sy, sz) = ((x1 - x0) / w as f32, (y1 - y0) / h as f32, (z1 - z0) / d as f32);
    // (texture origin, texels across and down, 3D corner at the texture
    // origin, one texel across, one texel down, normal)
    let faces: [((u32, u32), (u32, u32), V3, V3, V3, V3); 6] = [
        ((u + d, v + d), (w, h), [x0, y1, z1], [sx, 0.0, 0.0], [0.0, -sy, 0.0], [0.0, 0.0, 1.0]),
        ((u + 2 * d + w, v + d), (w, h), [x1, y1, z0], [-sx, 0.0, 0.0], [0.0, -sy, 0.0], [0.0, 0.0, -1.0]),
        ((u, v + d), (d, h), [x0, y1, z0], [0.0, 0.0, sz], [0.0, -sy, 0.0], [-1.0, 0.0, 0.0]),
        ((u + d + w, v + d), (d, h), [x1, y1, z1], [0.0, 0.0, -sz], [0.0, -sy, 0.0], [1.0, 0.0, 0.0]),
        ((u + d, v), (w, d), [x0, y1, z0], [sx, 0.0, 0.0], [0.0, 0.0, sz], [0.0, 1.0, 0.0]),
        ((u + d + w, v), (w, d), [x0, y0, z0], [sx, 0.0, 0.0], [0.0, 0.0, sz], [0.0, -1.0, 0.0]),
    ];
    for ((tu, tv), (nu, nv), o, du, dv, normal) in faces {
        let at = |a: u32, b: u32| add(o, add(mul(du, a as f32), mul(dv, b as f32)));
        for b in 0..nv {
            for a in 0..nu {
                let mut color = texel(img, tu + a, tv + b);
                if p.outer {
                    if color[3] == 0 {
                        continue;
                    }
                } else {
                    color[3] = 255;
                }
                out.push(Quad { corners: [at(a, b), at(a + 1, b), at(a + 1, b + 1), at(a, b + 1)], normal, color });
            }
        }
    }
}

fn add(a: V3, b: V3) -> V3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn mul(a: V3, s: f32) -> V3 {
    [a[0] * s, a[1] * s, a[2] * s]
}

fn dot(a: V3, b: V3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn norm(a: V3) -> V3 {
    mul(a, 1.0 / dot(a, a).sqrt())
}

/// Where a preview starts: a three-quarter view from slightly above.
const START: (f32, f32) = (-0.5, 0.18);

/// The preview widget: drag to turn it, double-click to reset. `turn` is
/// (yaw, pitch) away from the starting view.
pub fn show(ui: &mut Ui, t: &Theme, model: Option<&Model>, turn: &mut (f32, f32), size: egui::Vec2) -> egui::Response {
    let (rect, r) = ui.allocate_exact_size(size, Sense::click_and_drag());
    if r.dragged() {
        let d = r.drag_delta();
        turn.0 += d.x * 0.012;
        turn.1 = (turn.1 + d.y * 0.012).clamp(-1.2 - START.1, 1.2 - START.1);
    }
    if r.double_clicked() {
        *turn = (0.0, 0.0);
    }
    let p = ui.painter();
    p.rect_filled(rect, 0.0, t.fg_a(0.03));
    p.rect_stroke(rect, 0.0, egui::Stroke::new(1.0, t.fg_a(0.12)), egui::StrokeKind::Inside);
    match model {
        Some(m) => m.paint(p, rect.shrink(12.0), START.0 + turn.0, START.1 + turn.1),
        None => {
            ui.put(Rect::from_center_size(rect.center(), vec2(24.0, 24.0)), egui::Spinner::new());
        }
    }
    r.on_hover_cursor(egui::CursorIcon::Grab)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skin(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_pixel(w, h, image::Rgba([10, 20, 30, 255]))
    }

    /// Texels on a box's base faces.
    fn base(w: usize, h: usize, d: usize) -> usize {
        2 * (w * h + d * h + w * d)
    }

    #[test]
    fn texel_counts() {
        // A fully opaque 64×64 skin: every layer of every part is drawn.
        let classic = Model::build(&skin(64, 64), false, None).quads.len();
        let arm = base(4, 12, 4);
        let all = 2 * (base(8, 8, 8) + base(8, 12, 4) + 4 * arm);
        assert_eq!(classic, all);
        let slim = Model::build(&skin(64, 64), true, None).quads.len();
        assert_eq!(classic - slim, 4 * (arm - base(3, 12, 4)));
        // Legacy: no outer layers but the hat, which is hidden when solid.
        let legacy = Model::build(&skin(64, 32), false, None).quads.len();
        assert_eq!(legacy, base(8, 8, 8) + base(8, 12, 4) + 4 * arm);
        let cape = RgbaImage::from_pixel(64, 32, image::Rgba([1, 2, 3, 255]));
        assert_eq!(Model::build(&skin(64, 64), false, Some(&cape)).quads.len(), all + base(10, 16, 1));
    }

    #[test]
    fn faces_point_outwards_and_read_unmirrored() {
        // Mark the head's front texture's left column; it must land on the
        // viewer's left (−x) of the front face.
        let mut img = RgbaImage::from_pixel(64, 64, image::Rgba([0, 0, 0, 255]));
        for y in 8..16 {
            img.put_pixel(8, y, image::Rgba([255, 0, 0, 255]));
        }
        let m = Model::build(&img, false, None);
        let red: Vec<&Quad> = m.quads.iter().filter(|q| q.color[0] == 255).collect();
        assert_eq!(red.len(), 8);
        for q in red {
            assert_eq!(q.normal, [0.0, 0.0, 1.0]);
            assert!(q.corners.iter().all(|c| c[0] <= -3.0 && c[2] == 4.0));
        }
        // Every quad lies on the side of its box its normal points to.
        for q in &m.quads {
            let centre = q.corners.iter().fold([0.0; 3], |a, c| add(a, mul(*c, 0.25)));
            let axis = q.normal.iter().position(|n| *n != 0.0).unwrap();
            assert!(q.corners.iter().all(|c| c[axis] == centre[axis]));
        }
    }

    #[test]
    fn legacy_left_arm_mirrors_the_right() {
        let mut img = RgbaImage::from_pixel(64, 32, image::Rgba([0, 0, 0, 255]));
        // Right arm, front face, leftmost column.
        img.put_pixel(44, 20, image::Rgba([0, 255, 0, 255]));
        let m = Model::build(&img, false, None);
        let xs: Vec<f32> = m.quads.iter().filter(|q| q.color[1] == 255).map(|q| q.corners[0][0].min(q.corners[1][0])).collect();
        assert_eq!(xs.len(), 2);
        // Right arm: its outer edge at −8. Left arm: mirrored to the outer edge at +8.
        assert!(xs.contains(&-8.0) && xs.contains(&7.0), "{xs:?}");
    }

    #[test]
    fn cape_hangs_behind_the_back() {
        let cape = RgbaImage::from_pixel(64, 32, image::Rgba([1, 2, 3, 255]));
        let m = Model::build(&skin(64, 64), false, Some(&cape));
        let cape: Vec<&Quad> = m.quads.iter().filter(|q| q.color[0] == 1).collect();
        // Never inside the body (z −2…2), and swung out further at the bottom.
        assert!(cape.iter().all(|q| q.corners.iter().all(|c| c[2] <= -2.0 + 1e-4)));
        let low = cape.iter().flat_map(|q| q.corners).min_by(|a, b| a[1].total_cmp(&b[1])).unwrap();
        assert!(low[2] < -4.0, "{low:?}");
    }
}
