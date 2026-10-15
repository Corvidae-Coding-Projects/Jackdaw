//! Broker-fetched image resources and CPU-frame compositing.

use std::io::Cursor;

use anyhow::{Context as _, Result, ensure};
use blitz_dom::{LocalName, NodeId};
use vello_cpu::{Pixmap, color::PremulRgba8};

use crate::{DocumentRuntime, runtime_navigation::DocumentHost};

#[derive(Debug, Clone)]
pub(crate) struct DecodedImage {
    width: usize,
    height: usize,
    rgba: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImagePaintLayer {
    Background,
    ReplacedContent,
}

impl DocumentRuntime {
    pub(crate) fn install_network_images(
        &mut self,
        host: &dyn DocumentHost,
        base_url: &str,
    ) -> Result<()> {
        let base = browser_net::BrowserRequest::parse(base_url)?.url;
        for (node_id, relative_url) in self.document_images() {
            let Some(response) = base
                .join(&relative_url)
                .ok()
                .and_then(|url| host.fetch_subresource(url.as_str()).ok())
                .filter(|response| (200..=299).contains(&response.status))
            else {
                continue;
            };
            if let Ok(image) = decode_png(&response.body) {
                self.decoded_images.insert(node_id, (relative_url, image));
            }
        }
        Ok(())
    }

    pub(crate) fn document_images(&self) -> Vec<(NodeId, String)> {
        let mut stack: Vec<_> = self
            .document
            .root_node()
            .children
            .iter()
            .rev()
            .copied()
            .collect();
        let mut images = Vec::new();
        while let Some(node_id) = stack.pop() {
            let Some(node) = self.document.get_node(node_id) else {
                continue;
            };
            stack.extend(node.children.iter().rev().copied());
            let Some(element) = node.data.downcast_element() else {
                continue;
            };
            let source = if element.name.local.as_ref() == "img" {
                let configured_viewport_width = self.document.viewport().window_size.0;
                // Navigation can discover responsive resources before any frontend requests a
                // frame. Use the browser's documented 800px default viewport until a frontend
                // establishes a concrete width; treating the unset zero width as 1px incorrectly
                // selects narrow `srcset` candidates that do not correspond to the first frame.
                let viewport_width = if configured_viewport_width == 0 {
                    800
                } else {
                    configured_viewport_width
                };
                element
                    .attr(LocalName::from("srcset"))
                    .and_then(|srcset| select_width_candidate(srcset, viewport_width))
                    .or_else(|| {
                        element
                            .attr(LocalName::from("src"))
                            .map(str::trim)
                            .filter(|source| !source.is_empty())
                            .map(str::to_owned)
                    })
            } else {
                element
                    .attr(LocalName::from("style"))
                    .and_then(background_image_url)
            };
            if let Some(source) = source {
                images.push((node_id, source));
            }
        }
        images
    }

    pub(crate) fn composite_decoded_images(
        &self,
        pixels: &mut Pixmap,
        viewport_width: u16,
        viewport_height: u16,
        layer: ImagePaintLayer,
    ) {
        let output = pixels.data_mut();
        let output_width = usize::from(viewport_width);
        let viewport_right = f64::from(viewport_width);
        let viewport_bottom = f64::from(viewport_height);

        // Paint the exact candidate selected and fetched for this document generation. A later
        // frame request may resize the viewport, but that must not make already-decoded bytes
        // disappear merely because `srcset` would now select a different URL. A responsive
        // resource update belongs to the loader; painting consumes its committed result.
        for (node_id, (_, image)) in &self.decoded_images {
            let Some(node) = self.document.get_node(*node_id) else {
                continue;
            };
            let element = node.data.downcast_element();
            let is_replaced_image =
                element.is_some_and(|element| element.name.local.as_ref() == "img");
            if is_replaced_image != (layer == ImagePaintLayer::ReplacedContent) {
                continue;
            }
            let layout = node.final_layout();
            let position = node.absolute_position(0.0, 0.0);
            let bounds = self
                .document
                .get_client_bounding_rect(*node_id)
                .filter(|bounds| {
                    bounds.x.is_finite()
                        && bounds.y.is_finite()
                        && bounds.width.is_finite()
                        && bounds.height.is_finite()
                        && bounds.width > 0.0
                        && bounds.height > 0.0
                });
            let declared_dimension = |name: &str| {
                element
                    .and_then(|element| element.attr(LocalName::from(name)))
                    .and_then(|value| value.trim().parse::<f64>().ok())
                    .filter(|value| value.is_finite() && *value > 0.0)
            };
            let rendered_width = declared_dimension("width")
                .or_else(|| {
                    bounds
                        .filter(|bounds| bounds.width > 0.0)
                        .map(|bounds| bounds.width)
                })
                .or_else(|| (layout.size.width > 0.0).then_some(f64::from(layout.size.width)))
                .unwrap_or(image.width as f64);
            let rendered_height = declared_dimension("height")
                .or_else(|| {
                    bounds
                        .filter(|bounds| bounds.height > 0.0)
                        .map(|bounds| bounds.height)
                })
                .or_else(|| (layout.size.height > 0.0).then_some(f64::from(layout.size.height)))
                .unwrap_or(image.height as f64);
            // Blitz can expose different origins for a replaced element while rebuilding layout
            // after a viewport change. Prefer the resolved absolute position used by text painting,
            // but fall back to the client rectangle when that origin is non-finite or wholly outside
            // the requested frame and the client rectangle is visible. This keeps compositing and
            // hit-test geometry aligned without dropping already-decoded responsive resources.
            let absolute_origin = (f64::from(position.x), f64::from(position.y));
            let client_origin = bounds.map(|bounds| (bounds.x, bounds.y));
            let intersects_viewport = |(x, y): (f64, f64)| {
                x.is_finite()
                    && y.is_finite()
                    && x + rendered_width > self.scroll_x as f64
                    && y + rendered_height > self.scroll_y as f64
                    && x - (self.scroll_x as f64) < viewport_right
                    && y - (self.scroll_y as f64) < viewport_bottom
            };
            let (x, y) = if intersects_viewport(absolute_origin) {
                absolute_origin
            } else {
                client_origin
                    .filter(|origin| intersects_viewport(*origin))
                    .unwrap_or(absolute_origin)
            };
            let left = (x - self.scroll_x as f64)
                .floor()
                .clamp(0.0, viewport_right) as usize;
            let top = (y - self.scroll_y as f64)
                .floor()
                .clamp(0.0, viewport_bottom) as usize;
            let right = (x + rendered_width - self.scroll_x as f64)
                .ceil()
                .clamp(0.0, viewport_right) as usize;
            let bottom = (y + rendered_height - self.scroll_y as f64)
                .ceil()
                .clamp(0.0, viewport_bottom) as usize;
            if right <= left || bottom <= top {
                continue;
            }
            let destination_width = right - left;
            let destination_height = bottom - top;
            for y in top..bottom {
                let source_y = (y - top) * image.height / destination_height;
                for x in left..right {
                    let source_x = (x - left) * image.width / destination_width;
                    let source = (source_y * image.width + source_x) * 4;
                    let destination = y * output_width + x;
                    let alpha = u16::from(image.rgba[source + 3]);
                    if alpha == 0 {
                        continue;
                    }
                    let background = output[destination].to_u8_array();
                    let mut blended = [0_u8; 4];
                    for channel in 0..3 {
                        let foreground = u16::from(image.rgba[source + channel]);
                        blended[channel] = ((foreground * alpha
                            + u16::from(background[channel]) * (255 - alpha)
                            + 127)
                            / 255) as u8;
                    }
                    blended[3] = 255;
                    output[destination] = PremulRgba8::from_u8_array(blended);
                }
            }
        }
    }
}

fn background_image_url(style: &str) -> Option<String> {
    style.split(';').find_map(|declaration| {
        let (property, value) = declaration.split_once(':')?;
        if !property.trim().eq_ignore_ascii_case("background")
            && !property.trim().eq_ignore_ascii_case("background-image")
        {
            return None;
        }
        let value = value.trim();
        let start = value.find("url(")? + 4;
        let end = value[start..].find(')')? + start;
        let url = value[start..end]
            .trim()
            .trim_matches(|character| character == '\'' || character == '"');
        (!url.is_empty()).then(|| url.to_owned())
    })
}

fn select_width_candidate(srcset: &str, viewport_width: u32) -> Option<String> {
    let mut candidates = srcset.split(',').filter_map(|candidate| {
        let mut fields = candidate.split_whitespace();
        let source = fields.next()?.trim();
        let descriptor = fields.next()?;
        if source.is_empty() || fields.next().is_some() {
            return None;
        }
        let width = descriptor.strip_suffix('w')?.parse::<u32>().ok()?;
        (width > 0).then(|| (width, source.to_owned()))
    });
    let first = candidates.next()?;
    let mut selected = first.clone();
    for candidate in candidates {
        let candidate_fits = candidate.0 >= viewport_width;
        let selected_fits = selected.0 >= viewport_width;
        let candidate_is_better = match (candidate_fits, selected_fits) {
            (true, true) => candidate.0 < selected.0,
            (true, false) => true,
            (false, true) => false,
            (false, false) => candidate.0 > selected.0,
        };
        if candidate_is_better {
            selected = candidate;
        }
    }
    Some(selected.1)
}

fn decode_png(bytes: &[u8]) -> Result<DecodedImage> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().context("could not read PNG metadata")?;
    let buffer_size = reader.output_buffer_size();
    let mut decoded = vec![0; buffer_size];
    let info = reader
        .next_frame(&mut decoded)
        .context("could not decode PNG frame")?;
    let width = usize::try_from(info.width).context("PNG width does not fit usize")?;
    let height = usize::try_from(info.height).context("PNG height does not fit usize")?;
    ensure!(width > 0 && height > 0, "PNG dimensions must be nonzero");
    let samples = &decoded[..info.buffer_size()];
    let mut rgba = Vec::with_capacity(width * height * 4);
    match info.color_type {
        png::ColorType::Rgba => rgba.extend_from_slice(samples),
        png::ColorType::Rgb => {
            for sample in samples.chunks_exact(3) {
                rgba.extend_from_slice(&[sample[0], sample[1], sample[2], 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for sample in samples.chunks_exact(2) {
                rgba.extend_from_slice(&[sample[0], sample[0], sample[0], sample[1]]);
            }
        }
        png::ColorType::Grayscale => {
            for value in samples {
                rgba.extend_from_slice(&[*value, *value, *value, 255]);
            }
        }
        png::ColorType::Indexed => anyhow::bail!("expanded PNG retained indexed color data"),
    }
    ensure!(
        rgba.len() == width * height * 4,
        "decoded PNG pixel length does not match its dimensions"
    );
    Ok(DecodedImage {
        width,
        height,
        rgba,
    })
}
