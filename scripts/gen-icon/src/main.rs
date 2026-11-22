//! Rasterizes the geometry of assets/icon.svg and writes assets/icon.png and assets/waclient.ico.
//! Run from the repo root: cargo run --manifest-path scripts/gen-icon/Cargo.toml
use image::codecs::ico::{IcoEncoder, IcoFrame};
use image::{ColorType, RgbaImage};
use std::fs::File;

const SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];
const SS: u32 = 4; // supersampling per axis
const GREEN: [f32; 3] = [0x25 as f32, 0xD3 as f32, 0x66 as f32];
const WHITE: [f32; 3] = [255.0, 255.0, 255.0];
const W_POINTS: [(f32, f32); 5] = [(84.0, 95.0), (106.0, 146.0), (128.0, 108.0), (150.0, 146.0), (172.0, 95.0)];
const W_HALF_STROKE: f32 = 6.0;

fn in_round_rect(x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> bool {
    let cx = x.clamp(x0 + r, x1 - r);
    let cy = y.clamp(y0 + r, y1 - r);
    (x - cx).powi(2) + (y - cy).powi(2) <= r * r
}

fn in_triangle(p: (f32, f32), a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> bool {
    let s = |p1: (f32, f32), p2: (f32, f32), p3: (f32, f32)| {
        (p1.0 - p3.0) * (p2.1 - p3.1) - (p2.0 - p3.0) * (p1.1 - p3.1)
    };
    let (d1, d2, d3) = (s(p, a, b), s(p, b, c), s(p, c, a));
    !((d1 < 0.0 || d2 < 0.0 || d3 < 0.0) && (d1 > 0.0 || d2 > 0.0 || d3 > 0.0))
}

fn dist_to_segment(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    ((p.0 - (a.0 + t * dx)).powi(2) + (p.1 - (a.1 + t * dy)).powi(2)).sqrt()
}

/// Returns premultiplied-free RGBA for a point in the 256x256 design space.
fn shade(x: f32, y: f32) -> [f32; 4] {
    if (x - 128.0).powi(2) + (y - 128.0).powi(2) > 120.0 * 120.0 {
        return [0.0; 4];
    }
    let p = (x, y);
    let in_w = W_POINTS.windows(2).any(|s| dist_to_segment(p, s[0], s[1]) <= W_HALF_STROKE);
    if in_w {
        return [GREEN[0], GREEN[1], GREEN[2], 255.0];
    }
    let in_bubble = in_round_rect(x, y, 56.0, 68.0, 200.0, 170.0, 26.0)
        || in_triangle(p, (80.0, 160.0), (78.0, 198.0), (122.0, 166.0));
    let c = if in_bubble { WHITE } else { GREEN };
    [c[0], c[1], c[2], 255.0]
}

fn render(size: u32) -> RgbaImage {
    let mut img = RgbaImage::new(size, size);
    let scale = 256.0 / size as f32;
    let n = (SS * SS) as f32;
    for py in 0..size {
        for px in 0..size {
            let mut acc = [0.0f32; 4]; // alpha-weighted color + alpha
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) * scale;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) * scale;
                    let s = shade(x, y);
                    let a = s[3] / 255.0;
                    acc[0] += s[0] * a;
                    acc[1] += s[1] * a;
                    acc[2] += s[2] * a;
                    acc[3] += a;
                }
            }
            let px_out = if acc[3] > 0.0 {
                [(acc[0] / acc[3]).round() as u8, (acc[1] / acc[3]).round() as u8, (acc[2] / acc[3]).round() as u8, (acc[3] / n * 255.0).round() as u8]
            } else {
                [0, 0, 0, 0]
            };
            img.put_pixel(px, py, image::Rgba(px_out));
        }
    }
    img
}

fn main() {
    render(256).save("assets/icon.png").expect("write icon.png");
    let frames: Vec<IcoFrame> = SIZES
        .iter()
        .map(|&s| {
            let img = render(s);
            IcoFrame::as_png(img.as_raw(), s, s, ColorType::Rgba8.into()).expect("ico frame")
        })
        .collect();
    let file = File::create("assets/waclient.ico").expect("create ico");
    IcoEncoder::new(file).encode_images(&frames).expect("write ico");
}
