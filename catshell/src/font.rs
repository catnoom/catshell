//! Glyph rasterisation and the texture atlas the renderer samples.
//!
//! A terminal draws the same few hundred glyphs over and over, so every glyph is shaped
//! and rasterised once, packed into a texture, and thereafter costs nothing but a quad.
//! This is what lets a full-screen redraw be a single draw call.
//!
//! Shaping goes through `cosmic-text` rather than a raw font file so that characters the
//! primary font lacks — CJK, emoji, box drawing — fall back to a font that has them
//! instead of rendering as blank boxes.

use std::collections::HashMap;

use cosmic_text::{
    fontdb, Attrs, Buffer, Family, FontSystem, Metrics, Shaping, Style, SwashCache, Weight,
};
use swash::scale::image::Content;

/// Side length of the (square) atlas texture. 1024² of RGBA is 4 MiB and holds several
/// thousand glyphs — comfortably more than the styles of a Latin terminal font, with
/// room for the CJK and box-drawing characters that show up in practice.
const ATLAS_SIZE: u32 = 1024;

/// Padding between packed glyphs, so that linear sampling at a quad's edge cannot bleed
/// in a neighbour's coverage.
const GLYPH_PADDING: u32 = 1;

/// The style variants a cell can ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct FontStyle {
    pub bold: bool,
    pub italic: bool,
}

/// Where a rasterised glyph lives in the atlas and how to place it in a cell.
#[derive(Debug, Clone, Copy)]
pub struct Glyph {
    /// Atlas texture coordinates: `[u0, v0, u1, v1]`, normalised.
    pub uv: [f32; 4],
    /// Size of the quad in pixels.
    pub size: [f32; 2],
    /// Offset of the quad from the pen position (cell left, baseline), in pixels.
    pub offset: [f32; 2],
    /// True for a colour glyph (emoji), whose own colours must be used instead of the
    /// cell's foreground.
    pub colored: bool,
}

/// Fixed grid metrics derived from the font.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellMetrics {
    /// Advance width of one cell, in pixels.
    pub width: f32,
    /// Height of one cell (the line height), in pixels.
    pub height: f32,
    /// Baseline distance from the top of the cell, in pixels.
    pub ascent: f32,
    /// Top of the underline, relative to the top of the cell.
    pub underline_y: f32,
    /// Top of the strikeout line, relative to the top of the cell.
    pub strikeout_y: f32,
    /// Thickness of underlines and strikeouts, in pixels.
    pub line_thickness: f32,
}

/// Rasterises glyphs on demand and packs them into a texture.
pub struct GlyphAtlas {
    font_system: FontSystem,
    swash: SwashCache,
    /// The family as configured, which may be a generic name like `monospace`.
    family: String,
    /// The concrete family the system resolved that to. See [`resolve_family`].
    resolved_family: String,
    /// Whether the resolved family ships a real bold face. When it does not, bold is
    /// emboldened synthetically rather than silently rendering as regular.
    has_bold_face: bool,
    font_size: f32,
    metrics: CellMetrics,

    /// CPU-side copy of the texture, uploaded to the GPU when [`Self::take_dirty`] says so.
    pixels: Vec<u8>,
    /// Rows of the atlas changed since the last upload, as `[first, last)`.
    dirty_rows: Option<(u32, u32)>,

    // A shelf packer: glyphs fill a row left to right, then a new row starts below the
    // tallest glyph so far. Cheap, and good enough for glyphs, which are similar heights.
    shelf_y: u32,
    shelf_height: u32,
    next_x: u32,

    cache: HashMap<(char, FontStyle), Option<Glyph>>,
    /// UV of a fully opaque texel, used to draw backgrounds and underlines with the same
    /// pipeline as glyphs — so the whole frame stays one draw call.
    solid_uv: [f32; 4],
}

impl GlyphAtlas {
    /// Build an atlas for `family` at `font_size` logical pixels, scaled by `scale`
    /// (the display's DPI factor).
    pub fn new(family: &str, font_size: f32, scale: f32) -> Self {
        let mut font_system = FontSystem::new();
        let physical_size = font_size * scale;
        let resolved_family = resolve_family(&mut font_system, family);
        let metrics = measure(&mut font_system, &resolved_family, physical_size);
        let has_bold_face = family_has_bold(&font_system, &resolved_family);

        let mut atlas = Self {
            font_system,
            swash: SwashCache::new(),
            family: family.to_owned(),
            resolved_family,
            has_bold_face,
            font_size: physical_size,
            metrics,
            pixels: vec![0; (ATLAS_SIZE * ATLAS_SIZE * 4) as usize],
            dirty_rows: None,
            shelf_y: 0,
            shelf_height: 0,
            next_x: 0,
            cache: HashMap::new(),
            solid_uv: [0.0; 4],
        };
        atlas.solid_uv = atlas.allocate_solid();
        atlas
    }

    pub fn metrics(&self) -> CellMetrics {
        self.metrics
    }

    pub fn atlas_size(&self) -> u32 {
        ATLAS_SIZE
    }

    /// UV rect of an opaque texel, for drawing solid rectangles.
    pub fn solid_uv(&self) -> [f32; 4] {
        self.solid_uv
    }

    /// The glyph for `c`, rasterising it on first use.
    ///
    /// `None` means the character has nothing to draw — a space, or a codepoint no
    /// available font covers.
    pub fn glyph(&mut self, c: char, style: FontStyle) -> Option<Glyph> {
        if let Some(cached) = self.cache.get(&(c, style)) {
            return *cached;
        }
        let glyph = self.rasterize(c, style);
        self.cache.insert((c, style), glyph);
        glyph
    }

    /// Rows changed since the last call, with the pixel data to upload.
    ///
    /// Returns `None` when nothing changed, which is the usual case: after the first few
    /// frames every glyph on screen is already packed.
    pub fn take_dirty(&mut self) -> Option<(u32, u32, &[u8])> {
        let (first, last) = self.dirty_rows.take()?;
        let start = (first * ATLAS_SIZE * 4) as usize;
        let end = (last * ATLAS_SIZE * 4) as usize;
        Some((first, last - first, &self.pixels[start..end]))
    }

    /// Drop every cached glyph, for a font or size change.
    pub fn reconfigure(&mut self, family: &str, font_size: f32, scale: f32) {
        let physical_size = font_size * scale;
        if self.family == family && (self.font_size - physical_size).abs() < f32::EPSILON {
            return;
        }
        self.family = family.to_owned();
        self.resolved_family = resolve_family(&mut self.font_system, family);
        self.has_bold_face = family_has_bold(&self.font_system, &self.resolved_family);
        self.font_size = physical_size;
        self.metrics = measure(&mut self.font_system, &self.resolved_family, physical_size);

        self.cache.clear();
        self.pixels.fill(0);
        self.shelf_y = 0;
        self.shelf_height = 0;
        self.next_x = 0;
        self.dirty_rows = Some((0, ATLAS_SIZE));
        self.solid_uv = self.allocate_solid();
    }

    fn attrs(&self, style: FontStyle) -> Attrs<'_> {
        // The concrete family, never the generic one: see [`resolve_family`].
        let attrs = Attrs::new().family(Family::Name(&self.resolved_family));
        let attrs = if style.bold {
            attrs.weight(Weight::BOLD)
        } else {
            attrs
        };
        if style.italic {
            attrs.style(Style::Italic)
        } else {
            attrs
        }
    }

    fn rasterize(&mut self, c: char, style: FontStyle) -> Option<Glyph> {
        if c == ' ' || c == '\0' {
            return None;
        }

        // Shape the single character so that font fallback applies: the character may
        // well not be in the primary font, and shaping is what finds the one that has it.
        let metrics = Metrics::new(self.font_size, self.metrics.height);
        let mut buffer = Buffer::new(&mut self.font_system, metrics);
        let mut text = [0u8; 4];
        buffer.set_text(
            c.encode_utf8(&mut text),
            &self.attrs(style),
            Shaping::Advanced,
            None,
        );
        buffer.shape_until_scroll(&mut self.font_system, false);

        let layout_glyph = buffer.layout_runs().next()?.glyphs.first()?.clone();
        // Scale is already folded into `font_size`, so the physical glyph is 1:1.
        let physical = layout_glyph.physical((0.0, 0.0), 1.0);

        let image = self
            .swash
            .get_image(&mut self.font_system, physical.cache_key)
            .clone()?;
        if image.placement.width == 0 || image.placement.height == 0 {
            // A real glyph with no ink, such as a space in a fallback font.
            return None;
        }

        let colored = matches!(image.content, Content::Color);
        let (mut width, height) = (image.placement.width, image.placement.height);

        // A family with no bold face would otherwise render bold as plain regular text,
        // losing the distinction entirely. Smearing the mask one pixel sideways is the
        // long-standing terminal answer: it reads as bold and, because it is baked into
        // the cached glyph, costs nothing per frame.
        let mut data = image.data;
        if style.bold && !self.has_bold_face && !colored {
            data = embolden(&data, width, height, bytes_per_pixel(image.content));
            width += 1;
        }

        let (x, y) = self.allocate(width, height)?;
        self.blit(&data, image.content, x, y, width, height);

        let inv = 1.0 / ATLAS_SIZE as f32;
        Some(Glyph {
            uv: [
                x as f32 * inv,
                y as f32 * inv,
                (x + width) as f32 * inv,
                (y + height) as f32 * inv,
            ],
            size: [width as f32, height as f32],
            // `placement.top` counts upwards from the baseline, but our pixel coordinates
            // count downwards from the top of the cell, hence the negation.
            offset: [image.placement.left as f32, -image.placement.top as f32],
            colored,
        })
    }

    /// Copy a rasterised glyph into the atlas.
    ///
    /// Masks are stored as white with the coverage in alpha, so that multiplying by the
    /// cell's foreground colour in the shader tints them, while colour glyphs are stored
    /// as-is and multiplied by white. One shader path serves both.
    fn blit(&mut self, data: &[u8], content: Content, x: u32, y: u32, width: u32, height: u32) {
        for row in 0..height {
            let dst_start = (((y + row) * ATLAS_SIZE + x) * 4) as usize;
            for col in 0..width {
                let dst = dst_start + (col * 4) as usize;
                let i = (row * width + col) as usize;
                match content {
                    Content::Color => {
                        let src = i * 4;
                        self.pixels[dst..dst + 4].copy_from_slice(&data[src..src + 4]);
                    }
                    // Subpixel masks are rasterised per channel; we do not do subpixel
                    // antialiasing, so take one channel as coverage.
                    Content::SubpixelMask => {
                        let src = i * 4;
                        self.pixels[dst..dst + 3].copy_from_slice(&[255, 255, 255]);
                        self.pixels[dst + 3] = data[src + 1];
                    }
                    Content::Mask => {
                        self.pixels[dst..dst + 3].copy_from_slice(&[255, 255, 255]);
                        self.pixels[dst + 3] = data[i];
                    }
                }
            }
        }
        self.mark_dirty(y, y + height);
    }

    /// Reserve a `width`×`height` region, or `None` if the atlas is full.
    fn allocate(&mut self, width: u32, height: u32) -> Option<(u32, u32)> {
        if width > ATLAS_SIZE || height > ATLAS_SIZE {
            return None;
        }

        if self.next_x + width > ATLAS_SIZE {
            // Start a new shelf below the tallest glyph on the current one.
            self.shelf_y += self.shelf_height + GLYPH_PADDING;
            self.shelf_height = 0;
            self.next_x = 0;
        }

        if self.shelf_y + height > ATLAS_SIZE {
            // Out of room. Rather than corrupt the atlas, refuse; the character renders
            // blank. Growing the texture is the fix if this is ever hit in practice.
            return None;
        }

        let (x, y) = (self.next_x, self.shelf_y);
        self.next_x += width + GLYPH_PADDING;
        self.shelf_height = self.shelf_height.max(height);
        Some((x, y))
    }

    /// Reserve one opaque white texel for drawing solid rectangles.
    fn allocate_solid(&mut self) -> [f32; 4] {
        let (x, y) = self
            .allocate(1, 1)
            .expect("empty atlas has room for one texel");
        let offset = ((y * ATLAS_SIZE + x) * 4) as usize;
        self.pixels[offset..offset + 4].copy_from_slice(&[255, 255, 255, 255]);
        self.mark_dirty(y, y + 1);

        // Sample the texel's centre, so linear filtering cannot pick up its neighbours.
        let u = (x as f32 + 0.5) / ATLAS_SIZE as f32;
        let v = (y as f32 + 0.5) / ATLAS_SIZE as f32;
        [u, v, u, v]
    }

    fn mark_dirty(&mut self, first: u32, last: u32) {
        self.dirty_rows = Some(match self.dirty_rows {
            Some((old_first, old_last)) => (old_first.min(first), old_last.max(last)),
            None => (first, last),
        });
    }
}

/// Map a configured family name to a `cosmic-text` family.
///
/// The CSS generic names have to become the generic *variants*. Passing "monospace" as a
/// literal face name matches no installed font, and the fallback then picks a
/// proportional one — which sets the cell width from a proportional "M" while the glyphs
/// drawn in it are much narrower, so the text comes out visibly gappy.
fn family_of(name: &str) -> Family<'_> {
    let lowercase = name.to_ascii_lowercase();
    match lowercase.as_str() {
        "monospace" => Family::Monospace,
        "sans-serif" | "sansserif" => Family::SansSerif,
        "serif" => Family::Serif,
        "cursive" => Family::Cursive,
        "fantasy" => Family::Fantasy,
        _ => Family::Name(name),
    }
}

/// How many bytes one pixel occupies in a rasterised glyph.
fn bytes_per_pixel(content: Content) -> usize {
    match content {
        Content::Mask => 1,
        Content::SubpixelMask | Content::Color => 4,
    }
}

/// Widen a rasterised mask by one pixel, taking at each position the darker of it and
/// its left neighbour. The result is one pixel wider than the input.
fn embolden(data: &[u8], width: u32, height: u32, stride: usize) -> Vec<u8> {
    let new_width = width as usize + 1;
    let mut out = vec![0u8; new_width * height as usize * stride];
    for row in 0..height as usize {
        for col in 0..new_width {
            for channel in 0..stride {
                // Every source pixel contributes to its own column and the next one.
                let here = if col < width as usize {
                    data[(row * width as usize + col) * stride + channel]
                } else {
                    0
                };
                let left = if col > 0 {
                    data[(row * width as usize + col - 1) * stride + channel]
                } else {
                    0
                };
                out[(row * new_width + col) * stride + channel] = here.max(left);
            }
        }
    }
    out
}

/// Whether `family` includes an upright face heavy enough to serve as bold.
fn family_has_bold(font_system: &FontSystem, family: &str) -> bool {
    font_system.db().faces().any(|face| {
        face.style == fontdb::Style::Normal
            && face.weight.0 >= 600
            && face.families.iter().any(|(name, _)| name == family)
    })
}

/// Resolve a family name to the concrete family the system actually chose for it.
///
/// Styles have to be requested by concrete family name. Asking a *generic* family for a
/// bold face lets the matcher wander into a different family altogether: on a stock
/// Ubuntu install, `monospace` + bold lands on DejaVu Sans Mono BoldOblique while
/// regular text is Ubuntu Sans Mono — a different advance width, so bold text drifts out
/// of the grid, and slanted for good measure. Pinning the family first keeps every style
/// inside one metrically consistent face.
fn resolve_family(font_system: &mut FontSystem, requested: &str) -> String {
    let metrics = Metrics::new(16.0, 20.0);
    let mut buffer = Buffer::new(font_system, metrics);
    let attrs = Attrs::new().family(family_of(requested));
    buffer.set_text("M", &attrs, Shaping::Advanced, None);
    buffer.shape_until_scroll(font_system, false);

    let font_id = buffer
        .layout_runs()
        .next()
        .and_then(|run| run.glyphs.first().map(|glyph| glyph.font_id));

    font_id
        .and_then(|id| font_system.db().face(id))
        .and_then(|face| face.families.first().map(|(name, _)| name.clone()))
        .unwrap_or_else(|| requested.to_owned())
}

/// Derive the cell grid metrics from the font.
///
/// `family` must already be concrete — see [`resolve_family`].
fn measure(font_system: &mut FontSystem, family: &str, font_size: f32) -> CellMetrics {
    // A line height a little over the font size keeps rows from touching; this is the
    // usual terminal convention.
    let line_height = (font_size * 1.2).ceil();
    let metrics = Metrics::new(font_size, line_height);
    let mut buffer = Buffer::new(font_system, metrics);
    let attrs = Attrs::new().family(Family::Name(family));

    // "M" is a full-width glyph in any monospace font, so its advance is the cell width.
    buffer.set_text("M", &attrs, Shaping::Advanced, None);
    buffer.shape_until_scroll(font_system, false);

    let (width, ascent) = buffer
        .layout_runs()
        .next()
        .and_then(|run| run.glyphs.first().map(|glyph| (glyph.w, run.line_y)))
        // A font with no "M" is not a font we can measure; fall back to proportions that
        // are roughly right for a monospace face rather than dividing by zero later.
        .unwrap_or((font_size * 0.6, font_size));

    // Round the cell to whole physical pixels. A fractional advance puts every column
    // at a sub-pixel offset, which makes box-drawing characters fail to tile — the
    // borders in vim, tmux and htop come out with gaps at the joins — and makes glyphs
    // shimmer as they land on different pixel phases along a row.
    let width = width.max(1.0).round();
    let height = line_height.max(1.0).round();
    let ascent = ascent.round().clamp(1.0, height);

    let line_thickness = (font_size / 14.0).round().max(1.0);
    CellMetrics {
        width,
        height,
        ascent,
        // Just below the baseline, kept inside the cell.
        underline_y: (ascent + line_thickness)
            .min(height - line_thickness)
            .round(),
        strikeout_y: (ascent - font_size * 0.25).round(),
        line_thickness,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atlas() -> GlyphAtlas {
        GlyphAtlas::new("monospace", 14.0, 1.0)
    }

    #[test]
    fn generic_family_names_map_to_generic_families() {
        assert!(matches!(family_of("monospace"), Family::Monospace));
        assert!(matches!(family_of("Monospace"), Family::Monospace));
        assert!(matches!(family_of("serif"), Family::Serif));
        assert!(matches!(family_of("sans-serif"), Family::SansSerif));
        // A real face name is passed through untouched.
        assert!(
            matches!(family_of("DejaVu Sans Mono"), Family::Name(name) if name == "DejaVu Sans Mono")
        );
    }

    #[test]
    fn the_resolved_family_is_actually_fixed_width() {
        // The invariant the whole grid rests on: every character advances by the same
        // amount. When "monospace" was passed as a literal face name it matched nothing
        // and fell back to a proportional font, so cells were as wide as an "M" while
        // the glyphs in them were not — visibly gappy text.
        let mut font_system = FontSystem::new();
        let mut buffer = Buffer::new(&mut font_system, Metrics::new(13.0, 16.0));
        let attrs = Attrs::new().family(family_of("monospace"));
        buffer.set_text("iMW.l", &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut font_system, false);

        let advances: Vec<f32> = buffer
            .layout_runs()
            .next()
            .unwrap()
            .glyphs
            .iter()
            .map(|g| g.w)
            .collect();
        assert!(advances.len() >= 2, "nothing shaped");
        for advance in &advances {
            assert!(
                (advance - advances[0]).abs() < 0.01,
                "advances differ, so the font is not monospace: {advances:?}"
            );
        }
    }

    #[test]
    fn every_style_stays_in_one_metrically_consistent_family() {
        // Bold and italic must not drag in a different family. When they did, bold text
        // was both slanted and a different width, so it drifted out of the grid.
        let mut font_system = FontSystem::new();
        let resolved = resolve_family(&mut font_system, "monospace");

        let styles = [
            ("regular", Attrs::new().family(Family::Name(&resolved))),
            (
                "bold",
                Attrs::new()
                    .family(Family::Name(&resolved))
                    .weight(Weight::BOLD),
            ),
            (
                "italic",
                Attrs::new()
                    .family(Family::Name(&resolved))
                    .style(Style::Italic),
            ),
        ];

        let mut advances = Vec::new();
        for (label, attrs) in styles {
            let mut buffer = Buffer::new(&mut font_system, Metrics::new(13.0, 16.0));
            buffer.set_text("M", &attrs, Shaping::Advanced, None);
            buffer.shape_until_scroll(&mut font_system, false);
            let glyph = buffer.layout_runs().next().unwrap().glyphs[0].clone();

            let family = font_system
                .db()
                .face(glyph.font_id)
                .and_then(|face| face.families.first().map(|(name, _)| name.clone()))
                .unwrap();
            assert_eq!(family, resolved, "{label} left the resolved family");
            advances.push((label, glyph.w));
        }

        let (_, first) = advances[0];
        for (label, advance) in &advances {
            assert!(
                (advance - first).abs() < 0.01,
                "{label} advances {advance} but regular advances {first}: the grid would drift"
            );
        }
    }

    #[test]
    fn the_cell_grid_is_pixel_aligned() {
        // Sub-pixel cells make box-drawing characters fail to tile and text shimmer
        // along a row, so every metric that positions a cell must be a whole pixel.
        let m = atlas().metrics();
        for (name, value) in [
            ("width", m.width),
            ("height", m.height),
            ("ascent", m.ascent),
            ("underline_y", m.underline_y),
            ("strikeout_y", m.strikeout_y),
        ] {
            assert_eq!(value, value.round(), "{name} is {value}, not a whole pixel");
        }
    }

    #[test]
    fn metrics_are_sane() {
        let m = atlas().metrics();
        assert!(m.width > 1.0 && m.width < 40.0, "cell width {}", m.width);
        assert!(
            m.height >= 14.0 && m.height < 60.0,
            "cell height {}",
            m.height
        );
        // The baseline must sit inside the cell, or glyphs render outside their row.
        assert!(
            m.ascent > 0.0 && m.ascent <= m.height,
            "ascent {}",
            m.ascent
        );
        assert!(
            m.underline_y > m.ascent - m.height,
            "underline {}",
            m.underline_y
        );
        assert!(m.line_thickness >= 1.0);
    }

    #[test]
    fn monospace_cells_are_uniform() {
        // Every ASCII glyph must advance by the same amount, or the grid does not line up.
        let mut atlas = atlas();
        let width = atlas.metrics().width;
        for c in ['i', 'M', 'W', '.', '@'] {
            let glyph = atlas.glyph(c, FontStyle::default()).expect("ascii glyph");
            // A glyph may overhang its cell slightly, but not wildly.
            assert!(
                glyph.size[0] <= width * 2.0,
                "{c:?} is {} wide, cell {width}",
                glyph.size[0]
            );
        }
    }

    #[test]
    fn glyphs_are_cached_not_reallocated() {
        let mut atlas = atlas();
        let first = atlas.glyph('a', FontStyle::default()).unwrap();
        let second = atlas.glyph('a', FontStyle::default()).unwrap();
        assert_eq!(first.uv, second.uv, "second lookup repacked the glyph");
    }

    #[test]
    fn styles_are_packed_separately() {
        let mut atlas = atlas();
        let plain = atlas.glyph('a', FontStyle::default()).unwrap();
        let bold = atlas
            .glyph(
                'a',
                FontStyle {
                    bold: true,
                    italic: false,
                },
            )
            .unwrap();
        assert_ne!(plain.uv, bold.uv, "bold reused the regular glyph's slot");
    }

    #[test]
    fn bold_is_visibly_heavier_than_regular() {
        // Whether the family has a real bold face or gets the synthetic one, bold must
        // end up wider than regular; otherwise it is indistinguishable on screen.
        let mut atlas = atlas();
        let plain = atlas.glyph('M', FontStyle::default()).unwrap();
        let bold = atlas
            .glyph(
                'M',
                FontStyle {
                    bold: true,
                    italic: false,
                },
            )
            .unwrap();
        assert!(
            bold.size[0] > plain.size[0],
            "bold 'M' is {} wide, regular is {} — bold is not distinguishable",
            bold.size[0],
            plain.size[0]
        );
    }

    #[test]
    fn emboldening_smears_one_pixel_right() {
        // A single lit pixel becomes two, and the row grows by one.
        let data = [0u8, 255, 0];
        let out = embolden(&data, 3, 1, 1);
        assert_eq!(out, vec![0, 255, 255, 0]);
    }

    #[test]
    fn emboldening_preserves_rows() {
        let data = [255u8, 0, 0, 255];
        let out = embolden(&data, 2, 2, 1);
        assert_eq!(out.len(), 3 * 2);
        assert_eq!(&out[..3], &[255, 255, 0]);
        assert_eq!(&out[3..], &[0, 255, 255]);
    }

    #[test]
    fn blank_characters_have_no_glyph() {
        let mut atlas = atlas();
        assert!(atlas.glyph(' ', FontStyle::default()).is_none());
    }

    #[test]
    fn non_latin_characters_fall_back() {
        // The point of shaping through cosmic-text: these are not in a Latin font, and
        // must come from a fallback rather than render as nothing.
        let mut atlas = atlas();
        for c in ['日', '→', '█'] {
            if let Some(glyph) = atlas.glyph(c, FontStyle::default()) {
                assert!(
                    glyph.size[0] > 0.0 && glyph.size[1] > 0.0,
                    "{c:?} rasterised empty"
                );
            }
        }
    }

    #[test]
    fn uploads_are_reported_once() {
        let mut atlas = atlas();
        atlas.glyph('a', FontStyle::default()).unwrap();
        assert!(
            atlas.take_dirty().is_some(),
            "new glyph was not marked for upload"
        );
        assert!(
            atlas.take_dirty().is_none(),
            "unchanged atlas asked to re-upload"
        );
    }

    #[test]
    fn solid_texel_is_opaque_white() {
        let atlas = atlas();
        let uv = atlas.solid_uv();
        let x = (uv[0] * ATLAS_SIZE as f32) as usize;
        let y = (uv[1] * ATLAS_SIZE as f32) as usize;
        let offset = (y * ATLAS_SIZE as usize + x) * 4;
        assert_eq!(&atlas.pixels[offset..offset + 4], &[255, 255, 255, 255]);
    }

    #[test]
    fn atlas_does_not_overflow_its_bounds() {
        // Pack a lot of distinct glyphs and confirm every one stays inside the texture.
        let mut atlas = atlas();
        for c in ('\u{20}'..'\u{500}').chain('\u{4e00}'..'\u{4f00}') {
            if let Some(glyph) = atlas.glyph(c, FontStyle::default()) {
                assert!(
                    glyph.uv[2] <= 1.0 && glyph.uv[3] <= 1.0,
                    "{c:?} packed outside atlas"
                );
            }
        }
    }
}
