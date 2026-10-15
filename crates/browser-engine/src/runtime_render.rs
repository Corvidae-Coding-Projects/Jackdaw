//! Deterministic CPU rendering at the frontend-neutral document boundary.

use anyhow::{Result, ensure};
use blitz_dom::{NodeId, build_single_font_ctx};
use parley::{FontFamily, FontFamilyName, PositionedLayoutItem, fontique::GenericFamily};
use vello_cpu::{
    Glyph, Pixmap, RenderContext, Resources,
    color::{AlphaColor, PremulRgba8, Srgb},
    kurbo::Rect,
};

use crate::{
    runtime::{DocumentRuntime, ElementTarget},
    runtime_document::DEFAULT_FONT,
    runtime_images::ImagePaintLayer,
};

const CONTROL_FONT_FAMILY: &[FontFamilyName<'_>] =
    &[FontFamilyName::Generic(GenericFamily::SansSerif)];

impl DocumentRuntime {
    /// Rasterize the active document into a deterministic CPU viewport.
    ///
    /// A browser frame remains valid for an empty `about:blank` document, so document-level
    /// rendering uses a stable viewport instead of requiring the body to have nonzero content
    /// bounds. The body's computed background is painted when available.
    pub fn render_document(&mut self) -> Result<Pixmap> {
        self.render_document_at(800, 600)
    }

    /// Rasterize the active document at the frontend-requested viewport size.
    pub fn render_document_at(
        &mut self,
        viewport_width: u16,
        viewport_height: u16,
    ) -> Result<Pixmap> {
        // Frame production owns both the viewport and layout preconditions. Keep Blitz's CSS and
        // media-query viewport synchronized with the frontend-neutral frame before resolving;
        // otherwise a wide GUI can rasterize pixels at its requested size while the document is
        // still laid out against an earlier, narrower viewport.
        let requested_window_size = (u32::from(viewport_width), u32::from(viewport_height));
        let viewport_size_changed = self.document.viewport().window_size != requested_window_size;
        {
            let mut viewport = self.document.viewport_mut();
            viewport.window_size = requested_window_size;
        }
        let incremental_layout = self.document.incremental_layout();
        if viewport_size_changed {
            // Blitz synchronizes its style device when ViewportMut is dropped, but only
            // invalidates cached inline layout when scale changes. Force one complete public
            // layout pass so width-dependent wrapping and responsive geometry are rebuilt.
            self.document.set_incremental_layout(false);
        }
        self.resolve_layout();
        if viewport_size_changed {
            self.document.set_incremental_layout(incremental_layout);
        }

        let mut context = RenderContext::new(viewport_width, viewport_height);
        context.set_paint(AlphaColor::<Srgb>::new([1.0, 1.0, 1.0, 1.0]));
        context.fill_rect(&Rect::new(
            0.0,
            0.0,
            f64::from(viewport_width),
            f64::from(viewport_height),
        ));

        let viewport_width_f64 = f64::from(viewport_width);
        let viewport_height_f64 = f64::from(viewport_height);
        let mut stack: Vec<_> = self
            .document
            .root_node()
            .children
            .iter()
            .rev()
            .copied()
            .collect();
        while let Some(node_id) = stack.pop() {
            let Some(node) = self.document.get_node(node_id) else {
                continue;
            };
            stack.extend(node.children.iter().rev().copied());
            if node.data.downcast_element().is_none() {
                continue;
            }
            let Some(styles) = node.primary_styles() else {
                continue;
            };
            let Some(bounds) = self.document.get_client_bounding_rect(node_id) else {
                continue;
            };
            if bounds.width <= 0.0 || bounds.height <= 0.0 {
                continue;
            }
            let background = styles.resolve_color(&styles.get_background().background_color);
            // CSS propagates the root/body background to the canvas. The viewport frame is the
            // canvas exposed to frontends, so a body background must cover it even when the
            // body's content box is shorter than the viewport.
            let is_canvas_background = node
                .data
                .downcast_element()
                .is_some_and(|element| element.name.local.as_ref() == "body");
            let (left, top, right, bottom) = if is_canvas_background {
                (0.0, 0.0, viewport_width_f64, viewport_height_f64)
            } else {
                (
                    (bounds.x - self.scroll_x as f64).clamp(0.0, viewport_width_f64),
                    (bounds.y - self.scroll_y as f64).clamp(0.0, viewport_height_f64),
                    (bounds.x + bounds.width - self.scroll_x as f64).clamp(0.0, viewport_width_f64),
                    (bounds.y + bounds.height - self.scroll_y as f64)
                        .clamp(0.0, viewport_height_f64),
                )
            };
            if right <= left || bottom <= top {
                continue;
            }
            if background.alpha > 0.0 {
                let [red, green, blue] = background.components.to_array();
                context.set_paint(AlphaColor::<Srgb>::new([
                    red,
                    green,
                    blue,
                    background.alpha,
                ]));
                context.fill_rect(&Rect::new(left, top, right, bottom));
            }

            // Borders are independently paintable even when the element background is fully
            // transparent, which is common for form controls and outlined page structure. Layout
            // supplies the resolved widths; computed style supplies each side's CSS color.
            let layout = node.final_layout();
            let border = styles.get_border();
            let top_width = f64::from(layout.border.top).min(bottom - top);
            let right_width = f64::from(layout.border.right).min(right - left);
            let bottom_width = f64::from(layout.border.bottom).min(bottom - top);
            let left_width = f64::from(layout.border.left).min(right - left);

            if top_width > 0.0 {
                let color = styles.resolve_color(&border.clone_border_top_color());
                let [red, green, blue] = color.components.to_array();
                context.set_paint(AlphaColor::<Srgb>::new([red, green, blue, color.alpha]));
                context.fill_rect(&Rect::new(left, top, right, top + top_width));
            }
            if right_width > 0.0 {
                let color = styles.resolve_color(&border.clone_border_right_color());
                let [red, green, blue] = color.components.to_array();
                context.set_paint(AlphaColor::<Srgb>::new([red, green, blue, color.alpha]));
                context.fill_rect(&Rect::new(right - right_width, top, right, bottom));
            }
            if bottom_width > 0.0 {
                let color = styles.resolve_color(&border.clone_border_bottom_color());
                let [red, green, blue] = color.components.to_array();
                context.set_paint(AlphaColor::<Srgb>::new([red, green, blue, color.alpha]));
                context.fill_rect(&Rect::new(left, bottom - bottom_width, right, bottom));
            }
            if left_width > 0.0 {
                let color = styles.resolve_color(&border.clone_border_left_color());
                let [red, green, blue] = color.components.to_array();
                context.set_paint(AlphaColor::<Srgb>::new([red, green, blue, color.alpha]));
                context.fill_rect(&Rect::new(left, top, left + left_width, bottom));
            }
        }

        // Materialize the CSS box layer before resource images and glyphs. Background images are
        // then composited below text, while replaced `<img>` content is deliberately deferred
        // until after text. This preserves ordinary CSS paint order without coupling frontends to
        // the document implementation.
        context.flush();
        let mut pixels = Pixmap::new(viewport_width, viewport_height);
        let mut box_resources = Resources::new();
        context.render(&mut pixels, &mut box_resources);
        self.composite_decoded_images(
            &mut pixels,
            viewport_width,
            viewport_height,
            ImagePaintLayer::Background,
        );

        let mut context = RenderContext::new(viewport_width, viewport_height);
        let mut resources = Resources::new();
        context.set_paint(AlphaColor::<Srgb>::new([0.0, 0.0, 0.0, 1.0]));
        let mut text_roots: Vec<_> = self
            .document
            .root_node()
            .children
            .iter()
            .rev()
            .copied()
            .collect();
        while let Some(node_id) = text_roots.pop() {
            let Some(node) = self.document.get_node(node_id) else {
                continue;
            };
            text_roots.extend(node.children.iter().rev().copied());
            let Some(element) = node.data.downcast_element() else {
                continue;
            };
            let input_type = if element.name.local.as_ref() == "input" {
                element
                    .attr(blitz_dom::LocalName::from("type"))
                    .unwrap_or("text")
            } else {
                ""
            };
            let is_value_button = matches!(input_type, "button" | "submit" | "reset");
            let live_control_value = element.text_input_data().and_then(|input| {
                let value = input.editor.text().to_string();
                (!value.is_empty()).then_some(value)
            });
            let text_layout = if is_value_button || live_control_value.is_some() {
                None
            } else if let Some(input) = element.text_input_data() {
                // Preserve Blitz's editor layout for empty controls, where it may contain a
                // placeholder. Nonempty edited values are freshly shaped below: Blitz can retain
                // the pre-input glyph layout even after the editor text itself has advanced.
                input.editor.try_layout()
            } else {
                element
                    .inline_layout_data
                    .as_ref()
                    .map(|inline| &inline.layout)
            };
            let layout = node.final_layout();
            let position = node.absolute_position(0.0, 0.0);
            if let Some(text_layout) = text_layout {
                let origin_x = f64::from(position.x + layout.padding.left + layout.border.left)
                    - self.scroll_x as f64;
                let origin_y = f64::from(position.y + layout.padding.top + layout.border.top)
                    - self.scroll_y as f64;
                for line in text_layout.lines() {
                    for item in line.items() {
                        let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                            continue;
                        };
                        let mut style_node_id = Some(glyph_run.style().brush.id);
                        let text_color = loop {
                            let Some(candidate_id) = style_node_id else {
                                break None;
                            };
                            let Some(candidate) = self.document.get_node(candidate_id) else {
                                break None;
                            };
                            if let Some(styles) = candidate.primary_styles() {
                                break Some(styles.clone_color());
                            }
                            style_node_id = candidate.parent;
                        }
                        .or_else(|| node.primary_styles().map(|styles| styles.clone_color()));
                        let Some(text_color) = text_color else {
                            continue;
                        };
                        let [red, green, blue] = text_color.components.to_array();
                        context.set_paint(AlphaColor::<Srgb>::new([
                            red,
                            green,
                            blue,
                            text_color.alpha,
                        ]));
                        // Parley positions glyphs relative to the inline root's content box. Convert
                        // them to viewport coordinates explicitly: Vello's glyph preparation consumes
                        // these positions directly and must not depend on the path transform state.
                        let glyphs = glyph_run.positioned_glyphs().map(|glyph| Glyph {
                            id: glyph.id,
                            x: glyph.x + origin_x as f32,
                            y: glyph.y + origin_y as f32,
                        });
                        let glyph_result = context
                            .glyph_run(&mut resources, glyph_run.run().font())
                            .font_size(glyph_run.run().font_size())
                            .normalized_coords(glyph_run.run().normalized_coords())
                            .atlas_cache(true)
                            .fill_glyphs(glyphs);
                        glyph_result.map_err(|error| {
                            anyhow::anyhow!("failed to rasterize text glyphs: {error}")
                        })?;
                    }
                }
                continue;
            }

            // Value-backed buttons have no DOM text node, while Blitz can retain a stale glyph
            // layout for edited text controls. Shape both kinds of anonymous control text here, at
            // the shared document-paint boundary, so CLI screenshots and the GUI receive the same
            // current pixels without introducing frontend-specific web behavior.
            let (label, center_horizontally) = if is_value_button {
                let label = live_control_value.or_else(|| {
                    element
                        .attr(blitz_dom::LocalName::from("value"))
                        .filter(|value| !value.is_empty())
                        .map(str::to_owned)
                        .or_else(|| match input_type {
                            "submit" => Some("Submit".to_owned()),
                            "reset" => Some("Reset".to_owned()),
                            _ => None,
                        })
                });
                let Some(label) = label else {
                    continue;
                };
                (label, true)
            } else if let Some(value) = live_control_value {
                (value, false)
            } else {
                continue;
            };
            let Some(bounds) = self.document.get_client_bounding_rect(node_id) else {
                continue;
            };
            let available_width = (bounds.width as f32
                - layout.padding.left
                - layout.padding.right
                - layout.border.left
                - layout.border.right)
                .max(1.0);
            let mut font_context = build_single_font_ctx(DEFAULT_FONT);
            let mut layout_context: parley::LayoutContext<()> = parley::LayoutContext::new();
            let mut builder =
                layout_context.ranged_builder(&mut font_context, label.as_str(), 1.0, true);
            builder.push_default(FontFamily::from(CONTROL_FONT_FAMILY));
            builder.push_default(parley::StyleProperty::FontSize(13.0));
            let mut label_layout = builder.build(label.as_str());
            label_layout.break_all_lines(Some(available_width));

            let content_x = bounds.x - self.scroll_x as f64
                + f64::from(layout.border.left + layout.padding.left);
            let origin_x = if center_horizontally {
                bounds.x - self.scroll_x as f64
                    + ((bounds.width - f64::from(label_layout.width())) / 2.0).max(0.0)
            } else {
                content_x
            };
            let origin_y = bounds.y - self.scroll_y as f64
                + ((bounds.height - f64::from(label_layout.height())) / 2.0).max(0.0);
            if let Some(styles) = node.primary_styles() {
                let text_color = styles.clone_color();
                let [red, green, blue] = text_color.components.to_array();
                context.set_paint(AlphaColor::<Srgb>::new([
                    red,
                    green,
                    blue,
                    text_color.alpha,
                ]));
            }
            for line in label_layout.lines() {
                for item in line.items() {
                    let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                        continue;
                    };
                    let glyphs = glyph_run.positioned_glyphs().map(|glyph| Glyph {
                        id: glyph.id,
                        x: glyph.x + origin_x as f32,
                        y: glyph.y + origin_y as f32,
                    });
                    context
                        .glyph_run(&mut resources, glyph_run.run().font())
                        .font_size(glyph_run.run().font_size())
                        .normalized_coords(glyph_run.run().normalized_coords())
                        .atlas_cache(true)
                        .fill_glyphs(glyphs)
                        .map_err(|error| {
                            anyhow::anyhow!("failed to rasterize control-label glyphs: {error}")
                        })?;
                }
            }
        }

        context.flush();
        // `RenderContext::render` materializes a complete render target; rendering this second
        // pass directly into `pixels` would clear the box/background-image layer above. Render
        // glyph coverage into a transparent layer and composite it over the retained first pass.
        let mut text_pixels = Pixmap::new(viewport_width, viewport_height);
        context.render(&mut text_pixels, &mut resources);
        composite_over(&mut pixels, &mut text_pixels);
        self.composite_decoded_images(
            &mut pixels,
            viewport_width,
            viewport_height,
            ImagePaintLayer::ReplacedContent,
        );
        Ok(pixels)
    }

    /// Resolve the topmost visible element at a viewport coordinate using the same laid-out
    /// geometry and scroll offsets as frame rendering.
    pub fn hit_test(&mut self, x: i32, y: i32) -> Result<ElementTarget> {
        self.resolve_layout();
        ensure!(
            x >= 0 && y >= 0,
            "pointer coordinate is outside the viewport"
        );
        let document_x = f64::from(x) + self.scroll_x as f64;
        let document_y = f64::from(y) + self.scroll_y as f64;
        let mut stack: Vec<_> = self
            .document
            .root_node()
            .children
            .iter()
            .rev()
            .copied()
            .collect();
        let mut hit = None;
        while let Some(node_id) = stack.pop() {
            let Some(node) = self.document.get_node(node_id) else {
                continue;
            };
            stack.extend(node.children.iter().rev().copied());
            if node.data.downcast_element().is_none() {
                continue;
            }
            let Some(bounds) = self.document.get_client_bounding_rect(node_id) else {
                continue;
            };
            if document_x >= bounds.x
                && document_x < bounds.x + bounds.width
                && document_y >= bounds.y
                && document_y < bounds.y + bounds.height
            {
                hit = Some(node_id);
            }
        }
        let node_id =
            hit.ok_or_else(|| anyhow::anyhow!("pointer coordinate did not hit an element"))?;
        let existing_id = self.node_element_id(node_id);
        let element_id = self.ensure_element_id(node_id, existing_id);
        Ok(ElementTarget {
            document_generation: self.document_generation(),
            node_id,
            element_id,
        })
    }

    /// Rasterize the target's resolved layout box into a deterministic CPU frame.
    pub fn render_target(&self, target: &ElementTarget) -> Result<Pixmap> {
        self.validate_target(target)?;
        self.render_node(target.node_id)
    }

    fn render_node(&self, node_id: NodeId) -> Result<Pixmap> {
        let bounds = self
            .document
            .get_client_bounding_rect(node_id)
            .ok_or_else(|| anyhow::anyhow!("target has no resolved layout box"))?;
        ensure!(
            bounds.width > 0.0 && bounds.height > 0.0,
            "target resolved to an empty layout box"
        );
        let styles = self
            .document
            .get_node(node_id)
            .and_then(|node| node.primary_styles())
            .ok_or_else(|| anyhow::anyhow!("target has no resolved computed style"))?;
        let background = styles.resolve_color(&styles.get_background().background_color);
        let [red, green, blue] = background.components.to_array();
        let alpha = background.alpha;
        let width = bounds.width.ceil().clamp(1.0, 512.0) as u16;
        let height = bounds.height.ceil().clamp(1.0, 512.0) as u16;
        let mut context = RenderContext::new(width, height);
        context.set_paint(AlphaColor::<Srgb>::new([red, green, blue, alpha]));
        context.fill_rect(&Rect::new(0.0, 0.0, f64::from(width), f64::from(height)));
        context.flush();
        let mut resources = Resources::new();
        let mut pixels = Pixmap::new(width, height);
        context.render(&mut pixels, &mut resources);
        Ok(pixels)
    }
}

fn composite_over(destination: &mut Pixmap, source: &mut Pixmap) {
    for (destination, source) in destination
        .data_mut()
        .iter_mut()
        .zip(source.data_mut().iter())
    {
        let source = source.to_u8_array();
        let source_alpha = u16::from(source[3]);
        if source_alpha == 0 {
            continue;
        }
        let destination_channels = destination.to_u8_array();
        let inverse_alpha = 255 - source_alpha;
        let mut blended = [0_u8; 4];
        for channel in 0..3 {
            blended[channel] = (u16::from(source[channel])
                + (u16::from(destination_channels[channel]) * inverse_alpha + 127) / 255)
                .min(255) as u8;
        }
        blended[3] = (source_alpha
            + (u16::from(destination_channels[3]) * inverse_alpha + 127) / 255)
            .min(255) as u8;
        *destination = PremulRgba8::from_u8_array(blended);
    }
}
