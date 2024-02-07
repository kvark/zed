//todo!(linux) remove
#[allow(unused)]

use crate::{point, size, FontStyle, FontWeight, Point, ShapedGlyph};
use crate::{
    Bounds, DevicePixels, Font, FontFeatures, FontId, FontMetrics, FontRun, GlyphId, LineLayout,
    Pixels, PlatformTextSystem, RenderGlyphParams, SharedString, Size,
};
use anyhow::anyhow;
use anyhow::Ok;
use anyhow::Result;
use collections::HashMap;
use cosmic_text::fontdb::Query;
use cosmic_text::{
    Attrs, AttrsList, BufferLine, CacheKey, Family, Font as CosmicTextFont, FontSystem, SwashCache,
};
use parking_lot::{RwLock, RwLockUpgradableReadGuard};
use pathfinder_geometry::rect::RectF;
use pathfinder_geometry::rect::RectI;
use pathfinder_geometry::transform2d::Transform2F;
use pathfinder_geometry::vector::{Vector2F, Vector2I};
use smallvec::SmallVec;
use std::{borrow::Cow, sync::Arc};

pub(crate) struct LinuxTextSystem(RwLock<LinuxTextSystemState>);

struct LinuxTextSystemState {
    swash_cache: SwashCache,
    font_system: FontSystem,
    fonts: Vec<Arc<CosmicTextFont>>,
    font_selections: HashMap<Font, FontId>,
    font_ids_by_postscript_name: HashMap<String, FontId>,
    font_ids_by_family_name: HashMap<SharedString, SmallVec<[FontId; 4]>>,
    postscript_names_by_font_id: HashMap<FontId, String>,
}

unsafe impl Send for LinuxTextSystemState {}
unsafe impl Sync for LinuxTextSystemState {}

impl LinuxTextSystem {
    pub(crate) fn new() -> Self {
        Self(RwLock::new(LinuxTextSystemState {
            font_system: FontSystem::new(),
            swash_cache: SwashCache::new(),
            fonts: Vec::new(),
            font_selections: HashMap::default(),
            font_ids_by_postscript_name: HashMap::default(),
            font_ids_by_family_name: HashMap::default(),
            postscript_names_by_font_id: HashMap::default(),
        }))
    }
}

impl Default for LinuxTextSystem {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(unused)]
impl PlatformTextSystem for LinuxTextSystem {
    fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        self.0.write().add_fonts(fonts)
    }
    fn all_font_names(&self) -> Vec<String> {
        Vec::new()
    }
    fn all_font_families(&self) -> Vec<String> {
        Vec::new()
    }
    fn font_id(&self, font: &Font) -> Result<FontId> {
        let lock = self.0.upgradable_read();
        if let Some(font_id) = lock.font_selections.get(font) {
            Ok(*font_id)
        } else {
            let mut lock = RwLockUpgradableReadGuard::upgrade(lock);
            let candidates = if let Some(font_ids) = lock.font_ids_by_family_name.get(&font.family)
            {
                font_ids.as_slice()
            } else {
                let font_ids = lock.load_family(&font.family, font.features)?;
                lock.font_ids_by_family_name
                    .insert(font.family.clone(), font_ids);
                lock.font_ids_by_family_name[&font.family].as_ref()
            };

            let id = lock
                .font_system
                .db()
                .query(&Query {
                    families: &[Family::Name(&font.family)],
                    weight: font.weight.into(),
                    style: font.style.into(),
                    stretch: Default::default(),
                })
                .unwrap();
            println!("{:?}", id);
            println!("{:?}", lock.fonts);
            let font_id = if let Some(font_id) = lock.fonts.iter().position(|font| font.id() == id)
            {
                FontId(font_id)
            } else {
                // HACK: font isn't in fonts so add it there, this is because we query all the fonts in the db and maybe we haven't loaded it yet
                let font_id = FontId(lock.fonts.len());
                let font = lock.font_system.get_font(id).unwrap();
                lock.fonts.push(font);
                font_id
            };

            lock.font_selections.insert(font.clone(), font_id);
            Ok(font_id)
        }
    }
    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        let metrics = self.0.read().fonts[font_id.0].as_swash().metrics(&[]);
        FontMetrics {
            units_per_em: metrics.units_per_em as u32,
            ascent: metrics.ascent,
            descent: metrics.descent,
            line_gap: metrics.leading,
            underline_position: metrics.underline_offset,
            underline_thickness: metrics.stroke_size,
            cap_height: metrics.cap_height,
            x_height: metrics.x_height,
            bounding_box: Bounds {
                origin: point(0.0, 0.0),
                size: size(metrics.max_width, metrics.ascent + metrics.descent),
            }, // most probably incorrect
        }
    }
    fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
        let metrics = self.0.read().fonts[font_id.0].as_swash().metrics(&[]);
        Ok(Bounds {
            origin: point(0.0, 0.0), // do we need an origin?
            size: size(metrics.average_width, metrics.ascent + metrics.descent), // this height is probably incorect
        })
    }
    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        self.0.read().advance(font_id, glyph_id)
    }
    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.0.read().glyph_for_char(font_id, ch)
    }
    fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        self.0.write().raster_bounds(params)
    }
    fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        self.0.write().rasterize_glyph(params, raster_bounds)
    }
    fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        self.0.write().layout_line(text, font_size, runs)
    }
    // looks like this isnt used anywhere
    fn wrap_line(
        &self,
        text: &str,
        font_id: FontId,
        font_size: Pixels,
        width: Pixels,
    ) -> Vec<usize> {
        unimplemented!()
    }
}
impl LinuxTextSystemState {
    fn add_fonts(&mut self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        // TODO: I think on mac we don't actually load at this point don't think we can do this here though
        let db = self.font_system.db_mut();
        for bytes in fonts {
            match bytes {
                Cow::Borrowed(embedded_font) => {
                    db.load_font_data(embedded_font.to_vec());
                }
                Cow::Owned(bytes) => {
                    db.load_font_data(bytes);
                }
            }
        }
        Ok(())
    }

    fn load_family(
        &mut self,
        name: &SharedString,
        _features: FontFeatures,
    ) -> Result<SmallVec<[FontId; 4]>> {
        let mut font_ids = SmallVec::new();
        let family = self
            .font_system
            .get_font_matches(Attrs::new().family(cosmic_text::Family::Name(name)));
        for font in family.as_ref() {
            let font = self.font_system.get_font(*font).unwrap();
            // open_type::apply_features(&mut font, features);
            if font.as_swash().charmap().map('m') == 0 {
                self.font_system.db_mut().remove_face(font.id());
                continue;
            };
            // We've seen a number of panics in production caused by calling font.properties()
            // which unwraps a downcast to CFNumber. This is an attempt to avoid the panic,
            // and to try and identify the incalcitrant font.
            // let traits = font.native_font().all_traits();
            // if unsafe {
            //     !(traits
            //         .get(kCTFontSymbolicTrait)
            //         .downcast::<CFNumber>()
            //         .is_some()
            //         && traits
            //             .get(kCTFontWidthTrait)
            //             .downcast::<CFNumber>()
            //             .is_some()
            //         && traits
            //             .get(kCTFontWeightTrait)
            //             .downcast::<CFNumber>()
            //             .is_some()
            //         && traits
            //             .get(kCTFontSlantTrait)
            //             .downcast::<CFNumber>()
            //             .is_some())
            // } {
            //     log::error!(
            //         "Failed to read traits for font {:?}",
            //         font.postscript_name().unwrap()
            //     );
            //     continue;
            // }

            let font_id = FontId(self.fonts.len());
            font_ids.push(font_id);
            // let postscript_name = font
            //     .as_swash()
            //     .instances()
            //     .next()
            //     .unwrap()
            //     .postscript_name(None)
            //     .unwrap();
            // self.font_ids_by_postscript_name
            //     .insert(postscript_name.to_string(), font_id);
            // self.postscript_names_by_font_id
            //     .insert(font_id, postscript_name.to_string());
            self.fonts.push(font);
        }
        Ok(font_ids)
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        let width = self.fonts[font_id.0]
            .as_swash()
            .glyph_metrics(&[])
            .advance_width(glyph_id.0 as u16);
        let height = self.fonts[font_id.0]
            .as_swash()
            .glyph_metrics(&[])
            .advance_height(glyph_id.0 as u16);
        Ok(Size { width, height })
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        let glyph_id = self.fonts[font_id.0].as_swash().charmap().map(ch);
        if glyph_id == 0 {
            None
        } else {
            Some(GlyphId(glyph_id.into()))
        }
    }

    // fn id_for_native_font(&mut self, requested_font: Fre) -> FontId {
    //     let postscript_name = requested_font.postscript_name();
    //     if let Some(font_id) = self.font_ids_by_postscript_name.get(&postscript_name) {
    //         *font_id
    //     } else {
    //         let font_id = FontId(self.fonts.len());
    //         self.font_ids_by_postscript_name
    //             .insert(postscript_name.clone(), font_id);
    //         self.postscript_names_by_font_id
    //             .insert(font_id, postscript_name);
    //         self.fonts
    //             .push(font_kit::font::Font::from_core_graphics_font(
    //                 requested_font.copy_to_CGFont(),
    //             ));
    //         font_id
    //     }
    // }

    fn is_emoji(&self, font_id: FontId) -> bool {
        self.postscript_names_by_font_id
            .get(&font_id)
            .map_or(false, |postscript_name| {
                postscript_name == "AppleColorEmoji"
            })
    }

    // both raster functions have problems because I am not sure this is the correct mapping from cosmic text to gpui system
    fn raster_bounds(&mut self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        let font = &self.fonts[params.font_id.0];
        let scale = Transform2F::from_scale(params.scale_factor);
        let font_system = &mut self.font_system;
        let image = self
            .swash_cache
            .get_image(
                font_system,
                CacheKey::new(
                    font.id(),
                    params.glyph_id.0 as u16,
                    params.font_size.into(),
                    (0.0, 0.0),
                )
                .0,
            )
            .clone()
            .unwrap();
        Ok(Bounds {
            origin: point(image.placement.left.into(), (-image.placement.top).into()),
            size: size(image.placement.width.into(), image.placement.height.into()),
        })
    }

    fn rasterize_glyph(
        &mut self,
        params: &RenderGlyphParams,
        glyph_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        if glyph_bounds.size.width.0 == 0 || glyph_bounds.size.height.0 == 0 {
            Err(anyhow!("glyph bounds are empty"))
        } else {
            // Add an extra pixel when the subpixel variant isn't zero to make room for anti-aliasing.
            let mut bitmap_size = glyph_bounds.size;
            // if params.subpixel_variant.x > 0 {
            //     bitmap_size.width += DevicePixels(1);
            // }
            // if params.subpixel_variant.y > 0 {
            //     bitmap_size.height += DevicePixels(1);
            // }
            let bitmap_size = bitmap_size;
            let font = &self.fonts[params.font_id.0];
            let font_system = &mut self.font_system;
            let image = self
                .swash_cache
                .get_image(
                    font_system,
                    CacheKey::new(
                        font.id(),
                        params.glyph_id.0 as u16,
                        params.font_size.into(),
                        (0.0, 0.0),
                    )
                    .0,
                )
                .clone()
                .unwrap();

            Ok((bitmap_size, image.data))
        }
    }

    // HACK: This is all very incorrect, and maybe we should be using Buffer
    fn layout_line(&mut self, text: &str, font_size: Pixels, font_runs: &[FontRun]) -> LineLayout {
        let mut attrs_list = AttrsList::new(Attrs::new());
        let mut offs = 0;
        for run in font_runs {
            // need to be doing utf properly
            let font = &self.fonts[run.font_id.0];
            let font = self.font_system.db().face(font.id()).unwrap();
            attrs_list.add_span(
                offs..run.len,
                Attrs::new()
                    .family(Family::Name(&font.families.first().unwrap().0))
                    .stretch(font.stretch)
                    .style(font.style)
                    .weight(font.weight),
            );
            offs += run.len;
        }
        let mut line = BufferLine::new(text, attrs_list, cosmic_text::Shaping::Advanced);
        let layout = line.layout(
            &mut self.font_system,
            font_size.0,
            f32::MAX, // we don't have a width cause this should technically not be wrapped I believe
            cosmic_text::Wrap::None,
        );
        let mut runs = Vec::new();
        // what I think can happen is layout returns possibly multiple lines which means we should be probably working with it higher up in the text rendering
        let layout = layout.first().unwrap();
        for glyph in &layout.glyphs {
            let font_id = glyph.font_id;
            let font_id = FontId(
                self.fonts
                    .iter()
                    .position(|font| font.id() == font_id)
                    .unwrap(),
            );
            let mut glyphs = SmallVec::new();
            // this is definetly wrong, each glyph in glyphs from cosmic-text is a cluster with one glyph, ShapedRun takes a run of glyphs with the same font and direction
            glyphs.push(ShapedGlyph {
                id: GlyphId(glyph.glyph_id as u32),
                position: point(glyph.x.into(), glyph.y.into()),
                index: glyph.start,
                is_emoji: self.is_emoji(font_id),
            });
            runs.push(crate::ShapedRun { font_id, glyphs });
        }
        LineLayout {
            font_size,
            width: layout.w.into(),
            ascent: layout.max_ascent.into(),
            descent: layout.max_descent.into(),
            runs,
            len: text.len(),
        }
    }
}

impl From<RectF> for Bounds<f32> {
    fn from(rect: RectF) -> Self {
        Bounds {
            origin: point(rect.origin_x(), rect.origin_y()),
            size: size(rect.width(), rect.height()),
        }
    }
}

impl From<RectI> for Bounds<DevicePixels> {
    fn from(rect: RectI) -> Self {
        Bounds {
            origin: point(DevicePixels(rect.origin_x()), DevicePixels(rect.origin_y())),
            size: size(DevicePixels(rect.width()), DevicePixels(rect.height())),
        }
    }
}

impl From<Vector2I> for Size<DevicePixels> {
    fn from(value: Vector2I) -> Self {
        size(value.x().into(), value.y().into())
    }
}

impl From<RectI> for Bounds<i32> {
    fn from(rect: RectI) -> Self {
        Bounds {
            origin: point(rect.origin_x(), rect.origin_y()),
            size: size(rect.width(), rect.height()),
        }
    }
}

impl From<Point<u32>> for Vector2I {
    fn from(size: Point<u32>) -> Self {
        Vector2I::new(size.x as i32, size.y as i32)
    }
}

impl From<Vector2F> for Size<f32> {
    fn from(vec: Vector2F) -> Self {
        size(vec.x(), vec.y())
    }
}

impl From<FontWeight> for cosmic_text::Weight {
    fn from(value: FontWeight) -> Self {
        cosmic_text::Weight(value.0 as u16)
    }
}

impl From<FontStyle> for cosmic_text::Style {
    fn from(style: FontStyle) -> Self {
        match style {
            FontStyle::Normal => cosmic_text::Style::Normal,
            FontStyle::Italic => cosmic_text::Style::Italic,
            FontStyle::Oblique => cosmic_text::Style::Oblique,
        }
    }
}
