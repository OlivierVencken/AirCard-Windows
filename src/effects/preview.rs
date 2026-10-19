use eframe::egui::{self, Color32, Mesh, Pos2, Rect, TextureHandle, Vec2, pos2, vec2};

use super::{artwork::EffectsPreview, configuration::Selection, transaction};
use crate::i18n::Language;

const CARD_POINTS: f32 = 320.0;
const TRANSACTION_DELAY: f32 = 0.1;
const TRANSACTION_DURATION: f32 = 3.2;
const PARTICLE_RATE: f32 = 10.0;
const MAX_PARTICLE_LIFE: f32 = 6.0;

pub(super) struct Scene {
    selection: Selection,
    background: TextureHandle,
    foreground: TextureHandle,
    alternate: Option<TextureHandle>,
    particle: Option<TextureHandle>,
    transaction_sprite: Option<TextureHandle>,
    outline: Option<Outline>,
    canvas: Vec2,
    motion: Motion,
    #[cfg(test)]
    qa_frame: Option<Frame>,
}

#[derive(Clone, Copy, Default)]
struct Frame {
    tilt: Vec2,
    time: f32,
    transaction: Option<f32>,
}

#[derive(Default)]
struct Motion {
    frame: Frame,
}

impl Motion {
    fn advance(&mut self, dt: f32, target: Vec2) {
        let dt = dt.clamp(0.0, 0.05);
        self.frame.time += dt;
        let smoothing = 1.0 - (-dt / 0.12).exp();
        self.frame.tilt += (target - self.frame.tilt) * smoothing;
        if (self.frame.tilt - target).length() < 0.0001 {
            self.frame.tilt = target;
        }
        if let Some(elapsed) = &mut self.frame.transaction {
            *elapsed += dt;
            if *elapsed >= TRANSACTION_DELAY + TRANSACTION_DURATION {
                self.frame.transaction = None;
            }
        }
    }

    fn trigger(&mut self) {
        if self.frame.transaction.is_none() {
            self.frame.transaction = Some(0.0);
        }
    }
}

impl Scene {
    pub fn new(
        ctx: &egui::Context,
        artwork: &EffectsPreview,
        selection: Selection,
        particle: Option<TextureHandle>,
        transaction_sprite: Option<TextureHandle>,
        shape: Option<&transaction::Shape>,
    ) -> Self {
        let texture = |name, image: &image::RgbaImage| {
            ctx.load_texture(
                name,
                egui::ColorImage::from_rgba_unmultiplied(
                    [image.width() as usize, image.height() as usize],
                    image.as_raw(),
                ),
                egui::TextureOptions::LINEAR,
            )
        };
        Self {
            selection,
            background: texture("preview-background", &artwork.background),
            foreground: texture("preview-foreground", &artwork.foreground),
            alternate: artwork
                .alternate_background
                .as_ref()
                .map(|image| texture("preview-alternate", image)),
            particle,
            transaction_sprite,
            outline: shape.and_then(Outline::new),
            canvas: vec2(
                artwork.fallback.width() as f32,
                artwork.fallback.height() as f32,
            ),
            motion: Motion::default(),
            #[cfg(test)]
            qa_frame: None,
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, width: f32, language: Language) {
        let (rect, response) = ui.allocate_exact_size(
            vec2(width, width * self.canvas.y / self.canvas.x),
            egui::Sense::hover(),
        );
        let visible = ui.is_rect_visible(rect);
        if visible {
            let target = if self.selection.parallax || self.selection.cross_dissolve {
                response.hover_pos().map_or(Vec2::ZERO, |pointer| {
                    ((pointer - rect.center()) / (rect.size() * 0.5))
                        .clamp(vec2(-1.0, -1.0), vec2(1.0, 1.0))
                })
            } else {
                Vec2::ZERO
            };
            self.motion.advance(ui.input(|i| i.stable_dt), target);
            let frame = self.motion.frame;
            #[cfg(test)]
            let frame = self.qa_frame.unwrap_or(frame);
            self.paint(&ui.painter_at(rect), rect, frame);
            let animating = self.selection.particles
                || frame.transaction.is_some()
                || (frame.tilt - target).length() > 0.0001;
            if animating {
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(16));
            }
        }
        ui.add_space(6.0);
        if self.selection.parallax || self.selection.cross_dissolve {
            ui.small(language.text("Move your mouse over the card to tilt."))
                .on_hover_text(
                    language
                        .text("Motion and particle timing may differ on iPhone."),
                );
        }
        if self.selection.transaction
            && ui
                .add_enabled(
                    self.outline.is_some()
                        && self.transaction_sprite.is_some()
                        && self.motion.frame.transaction.is_none(),
                    egui::Button::new(language.text("Play transaction")),
                )
                .clicked()
        {
            self.motion.trigger();
            ui.ctx().request_repaint();
        }
    }

    fn paint(&self, painter: &egui::Painter, rect: Rect, frame: Frame) {
        let clip = rounded_outline(rect, 16.0_f32.min(rect.height() * 0.1));
        let (back, front) = layer_rects(rect, self.canvas, self.background.size_vec2(), frame.tilt);
        let (back, front) = if self.selection.parallax {
            (back, front)
        } else {
            (rect, rect)
        };
        paint_layer(painter, rect, &clip, back, &self.background, |_| 1.0);
        if self.selection.cross_dissolve
            && let Some(alternate) = &self.alternate
        {
            paint_layer(painter, rect, &clip, back, alternate, |p| {
                mask_alpha(p, back, rect.width() / CARD_POINTS, frame.tilt)
            });
        }
        // Native neutral emitters sit between the background and foreground.
        if self.selection.particles
            && let Some(sprite) = &self.particle
        {
            let mut mesh = Mesh::with_texture(sprite.id());
            for particle in twinkles(frame.time) {
                sprite_quad(&mut mesh, &clip, rect, sprite, particle, false);
            }
            painter.add(egui::Shape::mesh(mesh));
        }
        paint_layer(painter, rect, &clip, front, &self.foreground, |_| 1.0);
        if self.selection.transaction
            && let Some(elapsed) = frame.transaction
        {
            let age = elapsed - TRANSACTION_DELAY;
            if age >= 0.0
                && let (Some(sprite), Some(outline)) = (&self.transaction_sprite, &self.outline)
            {
                let fade = transaction_opacity(age);
                // The native dimming layer is black with alpha 0.5.
                let color = Color32::from_black_alpha((127.5 * fade).round() as u8);
                painter.add(egui::Shape::convex_polygon(
                    clip.clone(),
                    color,
                    egui::Stroke::NONE,
                ));
                let mut mesh = Mesh::with_texture(sprite.id());
                for particle in transaction_particles(age, outline) {
                    sprite_quad(&mut mesh, &clip, rect, sprite, particle, true);
                }
                painter.add(egui::Shape::mesh(mesh));
            }
        }
    }

    #[cfg(test)]
    pub fn qa_frame(&mut self, tilt: Vec2, transaction: Option<f32>) {
        self.qa_frame = Some(Frame {
            tilt,
            time: 7.25,
            transaction,
        });
    }
}

fn layer_rects(rect: Rect, canvas: Vec2, padded: Vec2, tilt: Vec2) -> (Rect, Rect) {
    // Assets already include the installed bleed. At rest the center pixels
    // exactly match the authored crop; at extremes only that bleed is exposed.
    let size = rect.size() * padded / canvas;
    let slide = (size - rect.size()) * 0.5;
    let offset = tilt * slide;
    (
        Rect::from_center_size(rect.center() - offset, size),
        Rect::from_center_size(rect.center() + offset, size),
    )
}

fn mask_alpha(point: Pos2, layer: Rect, point_scale: f32, tilt: Vec2) -> f32 {
    let side = layer.height().round() * 4.4;
    let center = layer.center() - tilt * (800.0 * point_scale);
    let scale = 0.65_f32.powf(tilt.length());
    // CAGradientLayer radial end point (1,1), relative to center (0.5,0.5).
    let radius = side * std::f32::consts::FRAC_1_SQRT_2 * scale;
    let distance = point.distance(center) / radius.max(0.001);
    // The three native colors are opaque/opaque/clear. Stop locations were not
    // recovered; use evenly spaced stops with a soft interpolation.
    1.0 - smoothstep(0.5, 1.0, distance)
}

fn smoothstep(start: f32, end: f32, value: f32) -> f32 {
    let x = ((value - start) / (end - start)).clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

fn rounded_outline(rect: Rect, radius: f32) -> Vec<Pos2> {
    let r = radius.min(rect.width() * 0.5).min(rect.height() * 0.5);
    let mut points = Vec::with_capacity(36);
    for (center, start) in [
        (
            pos2(rect.right() - r, rect.top() + r),
            -std::f32::consts::FRAC_PI_2,
        ),
        (pos2(rect.right() - r, rect.bottom() - r), 0.0),
        (
            pos2(rect.left() + r, rect.bottom() - r),
            std::f32::consts::FRAC_PI_2,
        ),
        (pos2(rect.left() + r, rect.top() + r), std::f32::consts::PI),
    ] {
        for step in 0..=8 {
            let angle = start + step as f32 / 8.0 * std::f32::consts::FRAC_PI_2;
            points.push(center + vec2(angle.cos(), angle.sin()) * r);
        }
    }
    points
}

// Clip textured primitives against the same rounded card, including emitters.
// A rectangular painter clip alone would leak sprites through rounded corners.
fn clip_polygon(mut polygon: Vec<Pos2>, clip: &[Pos2]) -> Vec<Pos2> {
    let mut output = Vec::with_capacity(polygon.len() + 4);
    for (a, b) in clip
        .iter()
        .zip(clip.iter().cycle().skip(1))
        .take(clip.len())
    {
        let edge = *b - *a;
        let distance = |p: Pos2| edge.x * (p.y - a.y) - edge.y * (p.x - a.x);
        if polygon.is_empty() {
            break;
        }
        output.clear();
        let mut previous = *polygon.last().unwrap();
        let mut previous_d = distance(previous);
        for &current in &polygon {
            let current_d = distance(current);
            if (previous_d >= 0.0) != (current_d >= 0.0) {
                let t = previous_d / (previous_d - current_d);
                output.push(previous + (current - previous) * t);
            }
            if current_d >= 0.0 {
                output.push(current);
            }
            previous = current;
            previous_d = current_d;
        }
        std::mem::swap(&mut polygon, &mut output);
    }
    polygon
}

fn append_polygon(
    mesh: &mut Mesh,
    polygon: &[Pos2],
    image_rect: Rect,
    color: impl Fn(Pos2) -> Color32,
) {
    if polygon.len() < 3 {
        return;
    }
    let start = mesh.vertices.len() as u32;
    for &p in polygon {
        let uv = (p - image_rect.min) / image_rect.size();
        mesh.vertices.push(egui::epaint::Vertex {
            pos: p,
            uv: pos2(uv.x.clamp(0.0, 1.0), uv.y.clamp(0.0, 1.0)),
            color: color(p),
        });
    }
    for i in 1..polygon.len() - 1 {
        mesh.indices
            .extend_from_slice(&[start, start + i as u32, start + i as u32 + 1]);
    }
}

fn paint_layer(
    painter: &egui::Painter,
    card: Rect,
    clip: &[Pos2],
    layer: Rect,
    texture: &TextureHandle,
    alpha: impl Fn(Pos2) -> f32,
) {
    let mut mesh = Mesh::with_texture(texture.id());
    // A modest fixed grid evaluates the moving radial mask on the GPU via
    // interpolated vertex alpha; no per-frame CPU image uploads/readbacks.
    for y in 0..24 {
        for x in 0..40 {
            let min = card.min + card.size() * vec2(x as f32 / 40.0, y as f32 / 24.0);
            let max = card.min + card.size() * vec2((x + 1) as f32 / 40.0, (y + 1) as f32 / 24.0);
            let tile = Rect::from_min_max(min, max);
            let radius = 16.0_f32.min(card.height() * 0.1);
            let near_corner = (tile.left() < card.left() + radius
                || tile.right() > card.right() - radius)
                && (tile.top() < card.top() + radius || tile.bottom() > card.bottom() - radius);
            let polygon = if near_corner {
                clip_polygon(
                    vec![
                        tile.left_top(),
                        tile.right_top(),
                        tile.right_bottom(),
                        tile.left_bottom(),
                    ],
                    clip,
                )
            } else {
                vec![
                    tile.left_top(),
                    tile.right_top(),
                    tile.right_bottom(),
                    tile.left_bottom(),
                ]
            };
            append_polygon(&mut mesh, &polygon, layer, |p| {
                Color32::from_white_alpha((alpha(p).clamp(0.0, 1.0) * 255.0).round() as u8)
            });
        }
    }
    painter.add(egui::Shape::mesh(mesh));
}

#[derive(Clone, Copy)]
struct Particle {
    position: Vec2,
    size_points: f32,
    alpha: f32,
}

fn random(seed: u32, salt: u32) -> f32 {
    let mut x = seed.wrapping_add(salt.wrapping_mul(0x9e3779b9));
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846ca68b);
    x ^= x >> 16;
    (x >> 8) as f32 / 16_777_216.0
}

fn twinkles(time: f32) -> Vec<Particle> {
    // Prewarm the surface so editing an input does not leave an empty emitter.
    let time = time + MAX_PARTICLE_LIFE;
    let last = (time * PARTICLE_RATE).floor() as u32;
    let first = last.saturating_sub((MAX_PARTICLE_LIFE * PARTICLE_RATE) as u32);
    (first..=last)
        .filter_map(|seed| {
            let age = time - seed as f32 / PARTICLE_RATE;
            let lifetime = (3.0 + (random(seed, 1) * 2.0 - 1.0) * 3.0).max(0.05);
            if age < 0.0 || age >= lifetime {
                return None;
            }
            let phase = age / lifetime;
            // CAEmitterBehavior curves are unresolved; this envelope implements the
            // recovered opacity + three-value scale behavior without adding physics.
            let envelope = (std::f32::consts::PI * phase).sin().max(0.0);
            Some(Particle {
                position: vec2(random(seed, 2), random(seed, 3)),
                size_points: 23.04 * envelope,
                alpha: smoothstep(0.0, 0.18, phase) * (1.0 - smoothstep(0.65, 1.0, phase)),
            })
        })
        .collect()
}

struct Outline {
    // Cumulative arc lengths avoid clustering emission on short path segments.
    segments: Vec<(f32, Vec2, Vec2)>,
    length: f32,
}

impl Outline {
    fn new(shape: &transaction::Shape) -> Option<Self> {
        let mut segments = Vec::new();
        let mut length = 0.0;
        for contour in &shape.contours {
            for pair in contour.windows(2) {
                let a = vec2(pair[0][0], pair[0][1]);
                let b = vec2(pair[1][0], pair[1][1]);
                // The native emitter path is normalized separately on x and y.
                // Weight by the reference card aspect for uniform visible length.
                let distance = ((b - a) * vec2(1.0, 969.0 / 1536.0)).length();
                if distance > 0.0 && distance.is_finite() {
                    length += distance;
                    segments.push((length, a, b));
                }
            }
        }
        (length > 0.0 && length.is_finite()).then_some(Self { segments, length })
    }

    fn sample(&self, fraction: f32) -> Vec2 {
        let distance = fraction.clamp(0.0, 1.0) * self.length;
        let index = self
            .segments
            .partition_point(|(end, _, _)| *end < distance)
            .min(self.segments.len() - 1);
        let (end, a, b) = self.segments[index];
        let start = if index == 0 {
            0.0
        } else {
            self.segments[index - 1].0
        };
        a + (b - a) * ((distance - start) / (end - start))
    }
}

fn transaction_opacity(age: f32) -> f32 {
    smoothstep(0.0, 0.15, age) * (1.0 - smoothstep(1.8, 2.7, age))
}

fn transaction_particles(age: f32, outline: &Outline) -> Vec<Particle> {
    if !(0.0..TRANSACTION_DURATION).contains(&age) {
        return Vec::new();
    }
    let zoom = 0.5 + 4.5 * smoothstep(0.0, TRANSACTION_DURATION, age);
    // 400 births/s through second one, then a decaying birth rate to zero.
    let last = (age.min(2.0) * 400.0).floor() as u32;
    (0..last)
        .filter_map(|seed| {
            let birth = seed as f32 / 400.0;
            if birth > 1.0 && random(seed, 8) > 2.0 - birth {
                return None;
            }
            let life = 2.0 + (random(seed, 1) * 2.0 - 1.0) * 0.5;
            let particle_age = age - birth;
            if particle_age < 0.0 || particle_age >= life {
                return None;
            }
            let phase = particle_age / life;
            let point = outline.sample(random(seed, 2));
            let direction = random(seed, 3) * std::f32::consts::TAU;
            let velocity = vec2(direction.cos(), direction.sin()) * 20.0 * particle_age;
            let position = vec2(0.5, 0.5)
                + (point - vec2(0.5, 0.5)) * zoom
                + velocity / vec2(CARD_POINTS, CARD_POINTS * 969.0 / 1536.0) * zoom;
            Some(Particle {
                position,
                size_points: 12.8 * (1.0 - phase * 0.8) * zoom,
                alpha: transaction_opacity(age) * (1.0 - smoothstep(0.6, 1.0, phase)),
            })
        })
        .collect()
}

fn sprite_quad(
    mesh: &mut Mesh,
    clip: &[Pos2],
    card: Rect,
    texture: &TextureHandle,
    particle: Particle,
    additive: bool,
) {
    let aspect = texture.size_vec2() / texture.size_vec2().max_elem();
    let size = aspect * particle.size_points * (card.width() / CARD_POINTS);
    if size.min_elem() <= 0.0 || particle.alpha <= 0.0 {
        return;
    }
    let rect = Rect::from_center_size(card.min + particle.position * card.size(), size);
    if !card.intersects(rect) {
        return;
    }
    let quad = vec![
        rect.left_top(),
        rect.right_top(),
        rect.right_bottom(),
        rect.left_bottom(),
    ];
    let radius = 16.0_f32.min(card.height() * 0.1);
    let inner = card.shrink(radius);
    let inside = card.contains_rect(rect)
        && quad.iter().all(|p| {
            let nearest = inner.clamp(*p);
            p.distance_sq(nearest) <= radius * radius
        });
    let polygon = if inside {
        quad
    } else {
        clip_polygon(quad, clip)
    };
    let brightness = (particle.alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
    let color = if additive {
        // egui's premultiplied blend treats zero-alpha colored vertices as light.
        Color32::from_rgba_premultiplied(brightness, brightness, brightness, 0)
    } else {
        Color32::from_white_alpha(brightness)
    };
    append_polygon(mesh, &polygon, rect, |_| color);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_paints_native_layer_order_and_mouse_input_only_changes_local_motion() {
        let ctx = egui::Context::default();
        let pixels = image::RgbaImage::from_pixel(22, 14, image::Rgba([40, 70, 100, 255]));
        let artwork = EffectsPreview {
            background: pixels.clone(),
            foreground: pixels.clone(),
            alternate_background: Some(pixels),
            fallback: image::RgbaImage::new(20, 12),
            sprite: None,
            transaction_sprite: None,
            transaction_shape: None,
        };
        let sprite = ctx.load_texture(
            "test-sprite",
            egui::ColorImage::new([2, 2], vec![Color32::WHITE; 4]),
            egui::TextureOptions::LINEAR,
        );
        let shape = transaction::Shape {
            contours: vec![vec![[0.2, 0.8], [0.5, 0.2], [0.8, 0.8], [0.2, 0.8]]],
        };
        let selection = Selection {
            parallax: true,
            cross_dissolve: true,
            particles: true,
            transaction: true,
        };
        let mut scene = Scene::new(
            &ctx,
            &artwork,
            selection,
            Some(sprite.clone()),
            Some(sprite.clone()),
            Some(&shape),
        );
        let card = Rect::from_min_size(pos2(20.0, 20.0), vec2(320.0, 192.0));
        let output = ctx.run(Default::default(), |ctx| {
            scene.paint(
                &ctx.layer_painter(egui::LayerId::background()),
                card,
                Frame {
                    tilt: vec2(0.45, -0.2),
                    time: 7.25,
                    transaction: Some(0.8),
                },
            );
        });
        let meshes: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::Shape::Mesh(mesh) => Some(mesh),
                _ => None,
            })
            .collect();
        let ids: Vec<_> = meshes.iter().map(|mesh| mesh.texture_id).collect();
        assert_eq!(
            ids,
            vec![
                scene.background.id(),
                scene.alternate.as_ref().unwrap().id(),
                sprite.id(),
                scene.foreground.id(),
                sprite.id()
            ]
        );
        for mesh in &meshes {
            assert!(mesh.is_valid());
            assert!(
                mesh.vertices
                    .iter()
                    .all(|v| card.expand(0.001).contains(v.pos)
                        && (0.0..=1.0).contains(&v.uv.x)
                        && (0.0..=1.0).contains(&v.uv.y))
            );
        }
        assert!(
            meshes[4].vertices.iter().all(|v| v.color.a() == 0),
            "Transaction particles use additive blending"
        );
        let frame = |events| egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(600.0, 500.0))),
            events,
            ..Default::default()
        };
        for _ in 0..4 {
            let _ = ctx.run(
                frame(vec![egui::Event::PointerMoved(pos2(300.0, 90.0))]),
                |ctx| {
                    egui::CentralPanel::default()
                        .show(ctx, |ui| scene.show(ui, 320.0, Language::English));
                },
            );
        }
        assert!(scene.motion.frame.tilt.x > 0.0);
        assert!(scene.motion.frame.tilt.y < 0.0);
        assert_eq!(
            scene.motion.frame.transaction, None,
            "Mouse tilt cannot trigger a payment preview"
        );
        assert_eq!(scene.selection, selection);
        let clock = scene.motion.frame.time;
        let _ = ctx.run(frame(vec![]), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.set_clip_rect(Rect::NOTHING);
                scene.show(ui, 320.0, Language::English);
            });
        });
        assert_eq!(
            scene.motion.frame.time, clock,
            "Offscreen previews must stop their clock"
        );
    }

    #[test]
    fn opposing_parallax_exposes_only_installed_bleed_and_mask_reveals_base() {
        let card = Rect::from_min_size(Pos2::ZERO, vec2(320.0, 201.875));
        let canvas = vec2(1536.0, 969.0);
        let padded = vec2(1690.0, 1065.0);
        for tilt in [Vec2::ZERO, vec2(-1.0, -1.0), vec2(1.0, 1.0)] {
            let (back, front) = layer_rects(card, canvas, padded, tilt);
            assert!(back.contains_rect(card));
            assert!(front.contains_rect(card));
            assert!(
                (back.center() + front.center().to_vec2() - card.center().to_vec2() * 2.0)
                    .to_vec2()
                    .length()
                    < 0.001
            );
        }
        assert_eq!(mask_alpha(card.center(), card, 1.0, Vec2::ZERO), 1.0);
        assert!(mask_alpha(card.center(), card, 1.0, vec2(1.0, 1.0)) < 0.01);
    }

    #[test]
    fn clock_return_to_center_and_transaction_replay_are_bounded() {
        let mut motion = Motion::default();
        motion.trigger();
        motion.advance(1.0, vec2(1.0, -1.0));
        assert_eq!(motion.frame.time, 0.05);
        let transaction = motion.frame.transaction;
        motion.trigger();
        assert_eq!(motion.frame.transaction, transaction);
        for _ in 0..100 {
            motion.advance(0.05, Vec2::ZERO);
        }
        assert!(motion.frame.transaction.is_none());
        assert_eq!(motion.frame.tilt, Vec2::ZERO);
        motion.trigger();
        assert_eq!(motion.frame.transaction, Some(0.0));
    }

    #[test]
    fn emitters_have_native_size_rate_bounds_and_sample_disconnected_paths() {
        let particles = twinkles(7.25);
        assert!(!particles.is_empty() && particles.len() <= 61);
        assert!(
            particles
                .iter()
                .all(|p| p.size_points <= 23.04 && (0.0..=1.0).contains(&p.alpha))
        );
        assert_ne!(twinkles(7.25)[0].size_points, twinkles(7.5)[0].size_points);
        let shape = transaction::Shape {
            contours: vec![vec![[0.0, 0.0], [1.0, 0.0]], vec![[0.0, 1.0], [1.0, 1.0]]],
        };
        let outline = Outline::new(&shape).unwrap();
        assert_eq!(outline.sample(0.25), vec2(0.5, 0.0));
        assert_eq!(outline.sample(0.75), vec2(0.5, 1.0));
        assert!(!transaction_particles(0.5, &outline).is_empty());
        assert!(transaction_particles(3.2, &outline).is_empty());
        assert!(transaction_particles(1.5, &outline).len() <= 800);
    }

    #[test]
    fn rounded_clip_preserves_sprite_uvs_and_excludes_corner_leaks() {
        let rect = Rect::from_min_size(Pos2::ZERO, vec2(100.0, 60.0));
        let clip = rounded_outline(rect, 16.0);
        let sprite = Rect::from_min_max(pos2(-5.0, -5.0), pos2(20.0, 20.0));
        let polygon = clip_polygon(
            vec![
                sprite.left_top(),
                sprite.right_top(),
                sprite.right_bottom(),
                sprite.left_bottom(),
            ],
            &clip,
        );
        assert!(
            polygon
                .iter()
                .all(|p| rect.expand(0.0001).contains(*p) && p.distance(Pos2::ZERO) > 4.0)
        );
        let mut mesh = Mesh::default();
        append_polygon(&mut mesh, &polygon, sprite, |_| Color32::WHITE);
        assert!(!mesh.indices.is_empty());
        assert!(
            mesh.vertices
                .iter()
                .all(|v| (0.0..=1.0).contains(&v.uv.x) && (0.0..=1.0).contains(&v.uv.y))
        );
    }
}
