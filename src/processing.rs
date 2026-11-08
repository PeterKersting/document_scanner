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
pub fn apply_color_mode(img: RgbImage, mode: ColorMode) -> DynamicImage {
    match mode {
        ColorMode::Color => DynamicImage::ImageRgb8(img),
        ColorMode::Gray => DynamicImage::ImageRgb8(img).into_luma8().into(),
        ColorMode::BlackWhite => {
            let gray = DynamicImage::ImageRgb8(img).into_luma8();
            let level = imageproc::contrast::otsu_level(&gray);
            imageproc::contrast::threshold(&gray, level, imageproc::contrast::ThresholdType::Binary).into()
        }
    }
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

    #[test]
    fn full_page_is_not_cropped() {
        let img = RgbImage::from_pixel(800, 1100, Rgb([250, 250, 250]));
        assert!(detect_document(&img).is_none());
    }
}
