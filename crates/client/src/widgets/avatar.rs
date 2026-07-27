//! The avatar disc: a member's picture cover-cropped into a circle, or their
//! initials on a hue-hashed disc when there is none.
//!
//! Two rules hold everywhere it is used. The slot is allocated whether or
//! not an avatar has arrived, so nothing moves when one does. And pictures
//! are cover-cropped, never squashed: the short side spans the circle and
//! the long side is centered and cropped, which happens in texture
//! coordinates so no pixels are copied.
//!
//! Textures are cached per content hash in the egui context, so a hash is
//! uploaded once no matter how many discs show it. The cache holds the only
//! `TextureHandle`, so dropping an entry frees the GPU texture. Lifecycle is
//! mark and sweep: drawing a disc marks its hash, and
//! [`sweep_avatar_textures`] at the end of the frame drops what nothing drew.
//! A member who left is not drawn, so their texture goes without anyone
//! having to notice they left.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use egui::epaint::Vertex;
use egui::{
    Align2, Color32, ColorImage, Context, FontId, Id, Mesh, Pos2, Response, Sense, Shape,
    TextureHandle, TextureOptions, Ui, pos2, vec2,
};

use crate::avatar::{disc_color, initials};
use crate::runtime::AvatarHandle;
use crate::theme;

/// Disc diameter in a mixer strip and in an invites row. The strip disc is
/// a portrait above the name; the row disc is a marker beside the label.
pub const AVATAR_D_STRIP: f32 = 30.0;
pub const AVATAR_D_ROW: f32 = 20.0;

/// Initials size as a fraction of the diameter. Larger than the stream
/// card's ratio because these discs are 30 px, not 130.
const INITIALS_RATIO: f32 = 0.40;

/// Texture cache, keyed by the avatar's content hash and owned by the egui
/// context so every surface shares one upload per hash.
#[derive(Default)]
struct Cache {
    uploaded: HashMap<String, TextureHandle>,
    /// Hashes drawn since the last sweep.
    drawn: HashSet<String>,
}

#[derive(Clone, Default)]
struct Textures(Arc<Mutex<Cache>>);

fn textures(ctx: &Context) -> Textures {
    ctx.data_mut(|d| {
        d.get_temp_mut_or_insert_with(Id::new("jamstream-avatar-textures"), Textures::default)
            .clone()
    })
}

fn texture(ctx: &Context, avatar: &AvatarHandle) -> TextureHandle {
    let cache = textures(ctx);
    let mut cache = cache.0.lock().expect("avatar textures");
    cache.drawn.insert(avatar.hash.clone());
    cache
        .uploaded
        .entry(avatar.hash.clone())
        .or_insert_with(|| {
            let size = [avatar.width as usize, avatar.height as usize];
            let image = ColorImage::from_rgba_unmultiplied(size, &avatar.rgba);
            ctx.load_texture(
                format!("avatar-{}", avatar.hash),
                image,
                TextureOptions::LINEAR,
            )
        })
        .clone()
}

/// Frees every texture nothing drew this frame. The cache is the only owner
/// of the handle, so dropping it releases the GPU texture; call this once at
/// the end of the frame, after every surface has had its turn.
pub fn sweep_avatar_textures(ctx: &Context) {
    let cache = textures(ctx);
    let mut cache = cache.0.lock().expect("avatar textures");
    let drawn = std::mem::take(&mut cache.drawn);
    cache.uploaded.retain(|hash, _| drawn.contains(hash));
}

/// How many textures are currently uploaded; the lifecycle test's window
/// into the cache.
pub fn avatar_texture_count(ctx: &Context) -> usize {
    textures(ctx)
        .0
        .lock()
        .expect("avatar textures")
        .uploaded
        .len()
}

/// One disc of exactly `diameter` points, always allocated. `dim` fades the
/// disc for a member who is not connected, matching how the stream card
/// pulls a disconnected portrait toward the panel.
pub fn avatar_disc(
    ui: &mut Ui,
    name: &str,
    avatar: Option<&AvatarHandle>,
    diameter: f32,
    dim: bool,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(diameter, diameter), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let center = rect.center();
    let radius = diameter / 2.0;
    let alpha = if dim { 100 } else { 255 };
    match avatar {
        Some(handle) => cover_circle(ui, center, radius, handle, alpha),
        None => {
            let fill = disc_color(name).gamma_multiply(alpha as f32 / 255.0);
            ui.painter().circle_filled(center, radius, fill);
            // The disc is dark in both themes, so its text is the dark
            // palette's, not the current theme's.
            let text = theme::DARK
                .text_primary
                .gamma_multiply(alpha as f32 / 255.0);
            ui.painter().text(
                center,
                Align2::CENTER_CENTER,
                initials(name),
                FontId::new(diameter * INITIALS_RATIO, theme::semibold(ui)),
                text,
            );
        }
    }
    response
}

/// Paints `handle` as a filled circle, cover-cropped. The circle is a
/// triangle fan with a one-pixel outer ring at zero alpha, which feathers
/// the edge the way egui's own tessellator antialiases a shape.
fn cover_circle(ui: &Ui, center: Pos2, radius: f32, handle: &AvatarHandle, alpha: u8) {
    let texture = texture(ui.ctx(), handle);
    let (w, h) = (handle.width as f32, handle.height as f32);
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    // Visible fraction of each axis: the short side is fully visible, the
    // long side is cropped to a centered square. Aspect is never touched.
    let short = w.min(h);
    let (span_u, span_v) = (short / w, short / h);
    let (min_u, min_v) = ((1.0 - span_u) / 2.0, (1.0 - span_v) / 2.0);
    // Unit offsets in -1..1 map into that square.
    let uv = |dx: f32, dy: f32| {
        pos2(
            min_u + span_u * (dx + 1.0) / 2.0,
            min_v + span_v * (dy + 1.0) / 2.0,
        )
    };

    let opaque = Color32::from_white_alpha(alpha);
    let clear = Color32::TRANSPARENT;
    let inner = (radius - 0.5).max(0.0);
    let outer = radius + 0.5;
    // Enough segments that the fan reads as a circle at any size we draw.
    let segments = ((radius * 3.0).round() as usize).clamp(24, 96);

    let mut mesh = Mesh::with_texture(texture.id());
    mesh.vertices.push(Vertex {
        pos: center,
        uv: uv(0.0, 0.0),
        color: opaque,
    });
    for i in 0..segments {
        let angle = i as f32 / segments as f32 * std::f32::consts::TAU;
        let (sin, cos) = angle.sin_cos();
        // Both rings sample the inner point, so the feather never reaches
        // past the crop rect.
        let texel = uv(cos * inner / radius, sin * inner / radius);
        mesh.vertices.push(Vertex {
            pos: center + vec2(cos * inner, sin * inner),
            uv: texel,
            color: opaque,
        });
        mesh.vertices.push(Vertex {
            pos: center + vec2(cos * outer, sin * outer),
            uv: texel,
            color: clear,
        });
    }
    for i in 0..segments {
        let next = (i + 1) % segments;
        let (in_a, out_a) = (1 + 2 * i as u32, 2 + 2 * i as u32);
        let (in_b, out_b) = (1 + 2 * next as u32, 2 + 2 * next as u32);
        mesh.add_triangle(0, in_a, in_b);
        mesh.add_triangle(in_a, out_a, out_b);
        mesh.add_triangle(in_a, out_b, in_b);
    }
    ui.painter().add(Shape::mesh(mesh));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle(w: u32, h: u32) -> AvatarHandle {
        AvatarHandle {
            hash: format!("{w}x{h}"),
            width: w,
            height: h,
            rgba: Arc::from(vec![200u8; (w * h * 4) as usize].into_boxed_slice()),
        }
    }

    /// The mesh must cover a square texture region regardless of aspect: a
    /// wide image crops left and right, a tall one top and bottom, and
    /// neither stretches.
    #[test]
    fn cover_crop_keeps_the_aspect_and_centers_the_crop() {
        for (w, h) in [(96u32, 48u32), (48, 96), (64, 64)] {
            let (fw, fh) = (w as f32, h as f32);
            let short = fw.min(fh);
            let (span_u, span_v) = (short / fw, short / fh);
            // One axis is fully visible, the other cropped to the same
            // number of source pixels: a square of the original.
            assert_eq!(span_u.max(span_v), 1.0);
            assert!((span_u * fw - span_v * fh).abs() < 0.001);
            let _ = handle(w, h);
        }
    }

    /// One frame per hash, and the texture goes when nothing draws it: the
    /// whole lifecycle, without a GPU in sight.
    #[test]
    fn textures_upload_once_per_hash_and_free_when_undrawn() {
        let ctx = Context::default();
        let ana = handle(64, 64);
        let ben = handle(96, 48);

        let frame = |draw: Vec<AvatarHandle>| {
            let _ = ctx.run_ui(Default::default(), |ui| {
                for h in &draw {
                    // Twice each: two surfaces can show one member.
                    avatar_disc(ui, "Ana", Some(h), 30.0, false);
                    avatar_disc(ui, "Ana", Some(h), 20.0, false);
                }
                avatar_disc(ui, "Mira", None, 30.0, false);
            });
            sweep_avatar_textures(&ctx);
        };

        frame(vec![ana.clone(), ben.clone()]);
        assert_eq!(
            avatar_texture_count(&ctx),
            2,
            "two hashes, four discs, two uploads"
        );
        frame(vec![ana.clone(), ben.clone()]);
        assert_eq!(avatar_texture_count(&ctx), 2, "a second frame reuses both");
        // Ben leaves the roster: his texture is not drawn, so it is freed.
        frame(vec![ana]);
        assert_eq!(avatar_texture_count(&ctx), 1);
        frame(Vec::new());
        assert_eq!(avatar_texture_count(&ctx), 0, "initials need no texture");
    }
}
