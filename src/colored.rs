// SPDX-License-Identifier: MPL-2.0-only

use colorgrad::Color;
use cosmic_bg_config::Gradient;
use image::Rgb32FImage;

/// Generate a background image from a color.
pub fn single(color: [f32; 3], width: u32, height: u32) -> Rgb32FImage {
    let mut imgbuf = Rgb32FImage::new(width, height);

    let pixel = image::Rgb(color);

    for x in 0..width {
        for y in 0..height {
            imgbuf.put_pixel(x, y, pixel);
        }
    }

    imgbuf
}

/// Generate a background image from a gradient.
pub fn gradient(
    gradient: &Gradient,
    width: u32,
    height: u32,
) -> Result<Rgb32FImage, colorgrad::CustomGradientError> {
    let mut colors = Vec::with_capacity(gradient.colors.len());

    for &[r, g, b] in &*gradient.colors {
        colors.push(colorgrad::Color::from_linear_rgba(
            f64::from(r),
            f64::from(g),
            f64::from(b),
            1.0,
        ));
    }

    let grad = colorgrad::CustomGradient::new()
        .colors(&colors)
        .mode(colorgrad::BlendMode::LinearRgb)
        .interpolation(colorgrad::Interpolation::Linear)
        .build()?;

    let mut imgbuf = image::ImageBuffer::new(width, height);

    let width = f64::from(width);
    let height = f64::from(height);

    let (dmin, dmax) = grad.domain();
    let angle = f64::from(gradient.radius.to_radians());

    // Map t which is in range [a, b] to range [c, d]
    #[allow(clippy::items_after_statements)]
    fn remap(t: f64, a: f64, b: f64, c: f64, d: f64) -> f64 {
        (t - a) * ((d - c) / (b - a)) + c
    }

    #[allow(clippy::cast_possible_truncation)]
    for (x, y, pixel) in imgbuf.enumerate_pixels_mut() {
        let (x, y) = (f64::from(x) - width / 2.0, f64::from(y) - height / 2.0);
        let pos = remap(
            x * f64::cos(angle) - y * f64::sin(angle),
            -width / 2.0,
            width / 2.0,
            dmin,
            dmax,
        );

        let Color { r, g, b, .. } = grad.at(pos);

        *pixel = image::Rgb([r as f32, g as f32, b as f32]);
    }

    Ok(imgbuf)
}
