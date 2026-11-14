//! Image post-processing: document edge detection, perspective correction and
//! colour reduction.

use crate::config::ColorMode;
use image::{imageops::FilterType, DynamicImage, GrayImage, Rgb, RgbImage};
use imageproc::contours::{find_contours, BorderType};
use imageproc::geometric_transformations::{warp_into, Border, Interpolation, Projection};
use imageproc::geometry::{approximate_polygon_dp, arc_length, contour_area, min_area_rect};
use imageproc::point::Point;

/// Corners of the detected page in image pixels: top-left, top-right,
/// bottom-right, bottom-left.
pub type Quad = [(f32, f32); 4];

const WORK_SIZE: u32 = 1000;

fn dist(a: (f32, f32), b: (f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

/// Orders four points as TL, TR, BR, BL.
fn order_quad(pts: &[(f32, f32)]) -> Quad {
    let by_sum = |p: &&(f32, f32)| ((p.0 + p.1) * 1000.0) as i64;
    let by_diff = |p: &&(f32, f32)| ((p.0 - p.1) * 1000.0) as i64;
    let tl = *pts.iter().min_by_key(by_sum).unwrap();
    let br = *pts.iter().max_by_key(by_sum).unwrap();
    let tr = *pts.iter().max_by_key(by_diff).unwrap();
    let bl = *pts.iter().min_by_key(by_diff).unwrap();
    [tl, tr, br, bl]
}

/// Interior angles in degrees must all be roughly right angles.
fn is_reasonable_quad(q: &Quad) -> bool {
    for i in 0..4 {
        let p = q[i];
        let a = q[(i + 3) % 4];
        let b = q[(i + 1) % 4];
        let v1 = (a.0 - p.0, a.1 - p.1);
        let v2 = (b.0 - p.0, b.1 - p.1);
        let n = (v1.0.hypot(v1.1)) * (v2.0.hypot(v2.1));
        if n < 1e-3 {
            return false;
        }
        let angle = ((v1.0 * v2.0 + v1.1 * v2.1) / n).clamp(-1.0, 1.0).acos().to_degrees();
        if !(65.0..=115.0).contains(&angle) {
            return false;
        }
    }
    true
}

fn quad_area(q: &Quad) -> f32 {
    let mut a = 0.0;
    for i in 0..4 {
        let (x1, y1) = q[i];
        let (x2, y2) = q[(i + 1) % 4];
        a += x1 * y2 - x2 * y1;
    }
    a.abs() / 2.0
}

/// Snaps each approximate corner to the nearby contour point that lies farthest
/// from the page centre (the true corner of a convex outline).
fn refine_corners(q: &Quad, contour: &[Point<i32>]) -> Quad {
    let cx = q.iter().map(|p| p.0).sum::<f32>() / 4.0;
    let cy = q.iter().map(|p| p.1).sum::<f32>() / 4.0;
    let min_side = (0..4).map(|i| dist(q[i], q[(i + 1) % 4])).fold(f32::MAX, f32::min);
    let radius = min_side * 0.15;
    q.map(|corner| {
        contour
            .iter()
            .map(|p| (p.x as f32, p.y as f32))
            .filter(|p| dist(*p, corner) <= radius)
            .max_by(|a, b| dist(*a, (cx, cy)).total_cmp(&dist(*b, (cx, cy))))
            .unwrap_or(corner)
    })
}

/// Detects the outline of a document on the scan.
///
/// Returns `None` when no clear page outline is found or when the page already
/// fills the whole image (nothing to crop).
pub fn detect_document(img: &RgbImage) -> Option<Quad> {
    let (w, h) = img.dimensions();
    let scale = (WORK_SIZE as f32 / w.max(h) as f32).min(1.0);
    let sw = ((w as f32 * scale).round() as u32).max(1);
    let sh = ((h as f32 * scale).round() as u32).max(1);
    let small: GrayImage =
        DynamicImage::ImageRgb8(image::imageops::resize(img, sw, sh, FilterType::Triangle)).to_luma8();
    let blurred = imageproc::filter::gaussian_blur_f32(&small, 1.5);
    let edges = imageproc::edges::canny(&blurred, 15.0, 45.0);
    let edges = imageproc::morphology::dilate(&edges, imageproc::distance_transform::Norm::LInf, 2);

    let image_area = (sw * sh) as f32;
    let mut best: Option<(f32, Quad)> = None;
    for contour in find_contours::<i32>(&edges) {
        if contour.border_type != BorderType::Outer || contour.points.len() < 20 {
            continue;
        }
        let pts = &contour.points;
        let area = contour_area(pts).abs() as f32;
        if area < image_area * 0.2 {
            continue;
        }
        let perimeter = arc_length(pts, true);
        let poly = approximate_polygon_dp(pts, 0.02 * perimeter, true);
        let quad = if poly.len() == 4 {
            Some(order_quad(&poly.iter().map(|p| (p.x as f32, p.y as f32)).collect::<Vec<_>>()))
        } else {
            // Rounded or slightly broken outline: use the minimal enclosing rectangle
            // if the contour fills it well.
            let r: [Point<i32>; 4] = min_area_rect(pts);
            let q = order_quad(&r.iter().map(|p| (p.x as f32, p.y as f32)).collect::<Vec<_>>());
            (area / quad_area(&q).max(1.0) > 0.85).then_some(q)
        };
        if let Some(q) = quad {
            let q = refine_corners(&q, pts);
            let qa = quad_area(&q);
            if is_reasonable_quad(&q) && best.is_none_or(|(a, _)| qa > a) {
                best = Some((qa, q));
            }
        }
    }

    let (_, q) = best?;
    // Nothing to do if the page touches all four image borders.
    let margin_x = sw as f32 * 0.02;
    let margin_y = sh as f32 * 0.02;
    let corners = [(0.0, 0.0), (sw as f32, 0.0), (sw as f32, sh as f32), (0.0, sh as f32)];
    let fills_image = q
        .iter()
        .zip(corners.iter())
        .all(|(p, c)| (p.0 - c.0).abs() < margin_x && (p.1 - c.1).abs() < margin_y);
    if fills_image {
        return None;
    }
    // The dilated edge lies slightly outside the page; pull the corners inwards
    // so no background line remains at the border.
    let cx = q.iter().map(|p| p.0).sum::<f32>() / 4.0;
    let cy = q.iter().map(|p| p.1).sum::<f32>() / 4.0;
    let inset = 4.0;
    let q = q.map(|(x, y)| {
        let d = ((cx - x).powi(2) + (cy - y).powi(2)).sqrt().max(1.0);
        (x + (cx - x) / d * inset * std::f32::consts::SQRT_2, y + (cy - y) / d * inset * std::f32::consts::SQRT_2)
    });
    let inv = 1.0 / scale;
    Some(q.map(|(x, y)| (x * inv, y * inv)))
}

/// Warps the quadrilateral `q` of `img` into an upright rectangle.
pub fn crop_to_quad(img: &RgbImage, q: &Quad) -> RgbImage {
    let out_w = dist(q[0], q[1]).max(dist(q[3], q[2])).round().max(1.0);
    let out_h = dist(q[0], q[3]).max(dist(q[1], q[2])).round().max(1.0);
    let dst = [(0.0, 0.0), (out_w, 0.0), (out_w, out_h), (0.0, out_h)];
    let Some(proj) = Projection::from_control_points(*q, dst) else {
        return img.clone();
    };
    let mut out = RgbImage::new(out_w as u32, out_h as u32);
    warp_into(img, proj, Interpolation::Bilinear, Border::Constant(Rgb([255, 255, 255])), &mut out);
    out
}

/// Converts the scan to the requested colour mode.
///
/// Black & white pages are kept in grayscale here; the threshold is chosen in
/// the preview and applied when saving (see [`threshold`]).
pub fn apply_color_mode(img: RgbImage, mode: ColorMode) -> DynamicImage {
    match mode {
        ColorMode::Color => DynamicImage::ImageRgb8(img),
        ColorMode::Gray | ColorMode::BlackWhite => DynamicImage::ImageRgb8(img).into_luma8().into(),
    }
}

/// Picks a black & white threshold for a document page.
///
/// Assumes that most of the page is paper and only a small part is ink: the
/// paper level is the median brightness, the ink level the darkest percent.
/// The threshold lies a bit past the middle between both, towards the paper,
/// so thin and light strokes stay black while paper texture turns white.
/// A 4 % margin is ignored (scanner shadows, feeder edges).
pub fn auto_threshold(img: &DynamicImage) -> u8 {
    let gray = img.to_luma8();
    let (w, h) = gray.dimensions();
    let step = ((w.max(h) / 1500).max(1)) as usize;
    let (mx, my) = (w / 25, h / 25);
    let mut hist = [0u64; 256];
    let mut total = 0u64;
    for y in (my..h.saturating_sub(my)).step_by(step) {
        for x in (mx..w.saturating_sub(mx)).step_by(step) {
            hist[gray.get_pixel(x, y)[0] as usize] += 1;
            total += 1;
        }
    }
    if total == 0 {
        return 128;
    }
    let percentile = |p: f64| -> i32 {
        let target = (total as f64 * p).ceil() as u64;
        let mut acc = 0;
        for (v, n) in hist.iter().enumerate() {
            acc += n;
            if acc >= target.max(1) {
                return v as i32;
            }
        }
        255
    };
    let paper = percentile(0.5);
    let ink = percentile(0.01);
    if paper - ink < 60 {
        // Hardly any ink (blank or very faint page): keep only clear marks.
        return (paper - 60).clamp(1, 254) as u8;
    }
    (ink as f32 + (paper - ink) as f32 * 0.6).round().clamp(1.0, 254.0) as u8
}

/// Estimates how far the lines of text on `img` are tilted and returns the
/// clockwise rotation in degrees (within ±`max_degrees`) that straightens them,
/// or `None` when the page has too little text to tell.
///
/// Projection profile: dark pixels are projected onto the vertical axis for
/// each candidate angle; the angle at which text lines line up best gives the
/// sharpest profile (largest squared change between neighbouring rows).
pub fn detect_skew(img: &DynamicImage, max_degrees: f32) -> Option<f32> {
    let level = auto_threshold(img);
    let gray = img.to_luma8();
    let (w, h) = gray.dimensions();
    let scale = (1500.0 / w.max(h) as f32).min(1.0);
    let (sw, sh) = (((w as f32 * scale) as u32).max(1), ((h as f32 * scale) as u32).max(1));
    let small = image::imageops::resize(&gray, sw, sh, FilterType::Triangle);
    let (mx, my) = (sw / 25, sh / 25);
    let mut ink: Vec<(f32, f32)> = Vec::new();
    for y in my..sh.saturating_sub(my) {
        for x in mx..sw.saturating_sub(mx) {
            if small.get_pixel(x, y)[0] < level {
                ink.push((x as f32 - sw as f32 / 2.0, y as f32 - sh as f32 / 2.0));
            }
        }
    }
    let area = ((sw - 2 * mx) * (sh - 2 * my)) as usize;
    if ink.len() < 500 || ink.len() > area * 2 / 5 {
        return None;
    }
    if ink.len() > 200_000 {
        let step = ink.len() / 200_000 + 1;
        ink = ink.into_iter().step_by(step).collect();
    }
    let rows = (sw as f32).hypot(sh as f32) as usize + 2;
    let offset = rows as f32 / 2.0;
    let mut bins = vec![0u32; rows];
    let mut score = |deg: f32| -> f64 {
        let (s, c) = deg.to_radians().sin_cos();
        bins.iter_mut().for_each(|b| *b = 0);
        for &(x, y) in &ink {
            // Row of the point after turning the page clockwise by `deg`.
            let r = (x * s + y * c + offset) as usize;
            bins[r.min(rows - 1)] += 1;
        }
        bins.windows(2)
            .map(|w| {
                let d = w[1] as f64 - w[0] as f64;
                d * d
            })
            .sum()
    };
    let best = |score: &mut dyn FnMut(f32) -> f64, from: f32, to: f32, step: f32| {
        let n = ((to - from) / step).round() as i32;
        (0..=n)
            .map(|i| from + i as f32 * step)
            .map(|a| (a, score(a)))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .unwrap()
    };
    let (coarse, _) = best(&mut score, -max_degrees, max_degrees, 0.25);
    let lo = (coarse - 0.25).max(-max_degrees);
    let hi = (coarse + 0.25).min(max_degrees);
    let (fine, top) = best(&mut score, lo, hi, 0.02);
    // A page without clear lines (photo, drawing) has a flat score curve.
    let base = score(fine + if fine > 0.0 { -2.0 } else { 2.0 });
    (top > base * 1.05).then_some((fine * 100.0).round() / 100.0)
}

/// Lookup table for brightness and contrast, both −100…100 (0 = unchanged).
/// Contrast stretches around mid-gray, brightness shifts the result.
pub fn tone_lut(brightness: f32, contrast: f32) -> [u8; 256] {
    let c = contrast.clamp(-100.0, 100.0) * 2.55;
    let factor = (259.0 * (c + 255.0)) / (255.0 * (259.0 - c));
    let shift = brightness.clamp(-100.0, 100.0) * 1.28;
    std::array::from_fn(|v| ((v as f32 - 128.0) * factor + 128.0 + shift).round().clamp(0.0, 255.0) as u8)
}

/// Applies brightness and contrast to a grayscale page.
pub fn adjust_gray(img: &DynamicImage, brightness: f32, contrast: f32) -> DynamicImage {
    let lut = tone_lut(brightness, contrast);
    let mut gray = img.to_luma8();
    gray.pixels_mut().for_each(|p| p.0[0] = lut[p.0[0] as usize]);
    DynamicImage::ImageLuma8(gray)
}

/// Turns a page into pure black and white: pixels darker than `level` become black.
pub fn threshold(img: &DynamicImage, level: u8) -> DynamicImage {
    let mut gray = img.to_luma8();
    gray.pixels_mut().for_each(|p| p.0[0] = if p.0[0] < level { 0 } else { 255 });
    DynamicImage::ImageLuma8(gray)
}

/// Prepares a raw scan: applies the colour mode and detects the page edges.
/// The cropped variant is computed up front so cropping can be switched on or
/// off after scanning without delay.
pub fn process_page(img: RgbImage, mode: ColorMode) -> crate::output::ScannedPage {
    let blank = is_blank(&img);
    let cropped = detect_document(&img).map(|q| apply_color_mode(crop_to_quad(&img, &q), mode));
    crate::output::ScannedPage {
        full: apply_color_mode(img, mode),
        cropped,
        blank,
    }
}

/// True if the page is (nearly) empty: hardly any dark pixels away from the
/// borders, where scanner shadows and feeder edges appear.
pub fn is_blank(img: &RgbImage) -> bool {
    let (w, h) = img.dimensions();
    let scale = (600.0 / w.max(h) as f32).min(1.0);
    let sw = ((w as f32 * scale) as u32).max(1);
    let sh = ((h as f32 * scale) as u32).max(1);
    let small = DynamicImage::ImageRgb8(image::imageops::resize(img, sw, sh, FilterType::Triangle)).to_luma8();
    let small = imageproc::filter::gaussian_blur_f32(&small, 0.8);
    // Ignore a 4 % margin all around.
    let (mx, my) = (sw / 25, sh / 25);
    let mut total = 0u32;
    let mut sum = 0u64;
    for y in my..sh.saturating_sub(my) {
        for x in mx..sw.saturating_sub(mx) {
            sum += small.get_pixel(x, y)[0] as u64;
            total += 1;
        }
    }
    if total == 0 {
        return true;
    }
    // Ink is anything clearly darker than the paper's average brightness.
    let paper = (sum / total as u64) as i32;
    let limit = (paper - 70).max(30) as u8;
    let mut dark = 0u32;
    for y in my..sh.saturating_sub(my) {
        for x in mx..sw.saturating_sub(mx) {
            if small.get_pixel(x, y)[0] < limit {
                dark += 1;
            }
        }
    }
    (dark as f32 / total as f32) < 0.0015
}

#[cfg(test)]
mod tests {
    use super::*;
    use imageproc::drawing::draw_polygon_mut;

    #[test]
    fn detects_tilted_page_on_dark_background() {
        let mut img = RgbImage::from_pixel(1200, 1600, Rgb([40, 40, 40]));
        // A 760 × 1160 page rotated by 4° around (600, 800).
        let (s, c) = 4f32.to_radians().sin_cos();
        let corners = [(-380.0, -580.0), (380.0, -580.0), (380.0, 580.0), (-380.0, 580.0)]
            .map(|(x, y): (f32, f32)| Point::new((600.0 + x * c - y * s) as i32, (800.0 + x * s + y * c) as i32));
        draw_polygon_mut(&mut img, &corners, Rgb([245, 245, 240]));
        let q = detect_document(&img).expect("page should be found");
        for (found, want) in q.iter().zip(corners.iter()) {
            assert!(
                (found.0 - want.x as f32).abs() < 25.0 && (found.1 - want.y as f32).abs() < 25.0,
                "{q:?}"
            );
        }
        let out = crop_to_quad(&img, &q);
        assert!(out.width() > 700 && out.height() > 1100);
    }

    #[test]
    fn detects_blank_pages() {
        let mut img = RgbImage::from_pixel(850, 1100, Rgb([238, 236, 230]));
        // scanner shadow at the border should not count
        for y in 0..1100 {
            for x in 0..12 {
                img.put_pixel(x, y, Rgb([40, 40, 40]));
            }
        }
        assert!(is_blank(&img));
        // a few lines of "text"
        for line in 0..6 {
            for y in 200 + line * 40..206 + line * 40 {
                for x in 120..600 {
                    img.put_pixel(x, y, Rgb([20, 20, 20]));
                }
            }
        }
        assert!(!is_blank(&img));
    }

    /// A page of `paper` brightness with some noise and `coverage` of it in `ink`.
    fn text_page(paper: u8, ink: u8, coverage: f32) -> DynamicImage {
        let mut img = image::GrayImage::from_pixel(1000, 1400, image::Luma([paper]));
        let mut seed = 7u32;
        for (x, y, p) in img.enumerate_pixels_mut() {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let noise = ((seed >> 16) % 13) as i32 - 6;
            // "Lines of text": short dashes on every 40th row band.
            let in_text = (y % 40) < 12 && (x % 25) < (25.0 * coverage * 40.0 / 12.0) as u32 && x > 80 && x < 920;
            let v = if in_text { ink as i32 } else { paper as i32 } + noise;
            p.0[0] = v.clamp(0, 255) as u8;
        }
        DynamicImage::ImageLuma8(img)
    }

    #[test]
    fn detects_skew_of_text_lines() {
        let page = text_page(240, 30, 0.05);
        let straight = detect_skew(&page, 5.0).expect("text lines found");
        assert!(straight.abs() <= 0.05, "straight page: {straight}");
        for tilt in [-3.0f32, -0.7, 1.5, 4.0] {
            let tilted = crate::output::rotate_degrees(&page, tilt, false);
            let fix = detect_skew(&tilted, 5.0).expect("text lines found");
            assert!((fix + tilt).abs() <= 0.1, "tilt {tilt}: correction {fix}");
        }
        assert_eq!(detect_skew(&text_page(240, 240, 0.0), 5.0), None, "blank page");
    }

    #[test]
    fn auto_threshold_separates_text_from_paper() {
        for (paper, ink) in [(245, 30), (230, 90), (240, 160), (200, 60)] {
            let page = text_page(paper, ink, 0.05);
            let t = auto_threshold(&page) as i32;
            assert!(t > ink as i32 + 10 && t < paper as i32 - 10, "paper {paper}, ink {ink}: {t}");
            let bw = threshold(&page, t as u8).into_luma8();
            let black = bw.pixels().filter(|p| p.0[0] == 0).count() as f32 / (bw.width() * bw.height()) as f32;
            assert!((0.02..0.08).contains(&black), "paper {paper}, ink {ink}: {black}");
        }
        // A blank page stays white.
        let blank = text_page(240, 240, 0.0);
        let bw = threshold(&blank, auto_threshold(&blank)).into_luma8();
        assert!(bw.pixels().all(|p| p.0[0] == 255));
    }

    #[test]
    fn tone_curve() {
        let same = tone_lut(0.0, 0.0);
        assert!(same.iter().enumerate().all(|(i, v)| *v as usize == i));
        let bright = tone_lut(50.0, 0.0);
        assert_eq!(bright[100], 164);
        let steep = tone_lut(0.0, 50.0);
        assert!(steep[64] < 64 && steep[192] > 192 && steep[128] == 128);
        let flat = tone_lut(0.0, -100.0);
        assert!(flat.iter().all(|v| *v == 128));
    }

    #[test]
    fn full_page_is_not_cropped() {
        let img = RgbImage::from_pixel(800, 1100, Rgb([250, 250, 250]));
        assert!(detect_document(&img).is_none());
    }
}
