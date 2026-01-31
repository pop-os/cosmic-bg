// SPDX-License-Identifier: MPL-2.0

use colorgrad::{Color, Gradient as ColorGradient};
use cosmic_bg_config::Gradient;
use image::Rgb32FImage;
use rayon::prelude::*;

/// Generate a background image from a color.
pub fn single(color: [f32; 3], width: u32, height: u32) -> Rgb32FImage {
    let pixel = image::Rgb(color);
    image::ImageBuffer::from_pixel(width, height, pixel)
}

/// Generate a background image from a gradient.
pub fn gradient(
    gradient: &Gradient,
    width: u32,
    height: u32,
) -> Result<Rgb32FImage, colorgrad::GradientBuilderError> {
    let mut colors = Vec::with_capacity(gradient.colors.len());
    for &[r, g, b] in &*gradient.colors {
        colors.push(colorgrad::Color::from_linear_rgba(r, g, b, 1.0));
    }

    let grad = colorgrad::GradientBuilder::new()
        .colors(&colors)
        .mode(colorgrad::BlendMode::LinearRgb)
        .build::<colorgrad::LinearGradient>()?;

    let mut imgbuf = image::ImageBuffer::new(width, height);
    let width = width as f32;
    let height = height as f32;
    let orientation = gradient.radius as u16;

    let (dmin, dmax) = grad.domain();
    let angle = gradient.radius.to_radians();
    let cos = f32::cos(angle);
    let sin = f32::sin(angle);
    const SCALE: f32 = 0.015;
    let w_scale = width / SCALE;
    let h_scale = height / SCALE;

    // Map t which is in range [a, b] to range [c, d]
    fn remap(t: f32, a: f32, b: f32, c: f32, d: f32) -> f32 {
        (t - a) * ((d - c) / (b - a)) + c
    }

    imgbuf.par_enumerate_pixels_mut().for_each(|(x, y, pixel)| {
        let x_f = x as f32;
        let y_f = y as f32;

        let pos = match orientation {
            0 => 1.0 - (y_f / height),
            90 => x_f / width,
            180 => y_f / height,
            270 => 1.0 - (x_f / width),
            _ => remap(
                (x_f - w_scale) * cos - (y_f - h_scale) * sin,
                -w_scale,
                w_scale,
                dmin,
                dmax,
            ),
        };

        let Color { r, g, b, .. } = grad.at(pos);
        *pixel = image::Rgb([r, g, b]);
    });

    Ok(imgbuf)
}
