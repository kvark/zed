use crate::{
    Bounds, Element, ElementContext, Hitbox, InteractiveElement, Interactivity, IntoElement,
    LayoutId, Pixels, SharedString, StyleRefinement, Styled, TransformationMatrix,
};
use usvg::{Point, Size};
use util::ResultExt;

/// An SVG element.
pub struct Svg {
    interactivity: Interactivity,
    transformation: Option<Transformation>,
    path: Option<SharedString>,
}

/// Create a new SVG element.
pub fn svg() -> Svg {
    Svg {
        interactivity: Interactivity::default(),
        transformation: None,
        path: None,
    }
}

impl Svg {
    /// Set the path to the SVG file for this element.
    pub fn path(mut self, path: impl Into<SharedString>) -> Self {
        self.path = Some(path.into());
        self
    }

    pub fn with_transformation(mut self, transformation: TransformationMatrix) -> Self {
        self.transformation = Some(transformation);
        self
    }
}

impl Element for Svg {
    type BeforeLayout = ();
    type AfterLayout = Option<Hitbox>;

    fn before_layout(&mut self, cx: &mut ElementContext) -> (LayoutId, Self::BeforeLayout) {
        let layout_id = self
            .interactivity
            .before_layout(cx, |style, cx| cx.request_layout(&style, None));
        (layout_id, ())
    }

    fn after_layout(
        &mut self,
        bounds: Bounds<Pixels>,
        _before_layout: &mut Self::BeforeLayout,
        cx: &mut ElementContext,
    ) -> Option<Hitbox> {
        self.interactivity
            .after_layout(bounds, bounds.size, cx, |_, _, hitbox, _| hitbox)
    }

    fn paint(
        &mut self,
        bounds: Bounds<Pixels>,
        _before_layout: &mut Self::BeforeLayout,
        hitbox: &mut Option<Hitbox>,
        cx: &mut ElementContext,
    ) where
        Self: Sized,
    {
        self.interactivity
            .paint(bounds, hitbox.as_ref(), cx, |style, cx| {
                if let Some((path, color)) = self.path.as_ref().zip(style.text.color) {
                    let transformation = self
                        .transformation
                        .map(|transformation| transformation.into_matrix(bounds.size))
                        .unwrap_or(TransformationMatrix::unit());

                    cx.paint_svg(bounds, path.clone(), transformation, color)
                        .log_err();
                }
            })
    }
}

impl IntoElement for Svg {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Styled for Svg {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.interactivity.base_style
    }
}

impl InteractiveElement for Svg {
    fn interactivity(&mut self) -> &mut Interactivity {
        &mut self.interactivity
    }
}

pub struct Transformation {
    scale: Size<Pixels>,
    translate: Point<Pixels>,
    rotate: f32,
}

impl Transformation {
    /// Create a new Transformation with the specified scale.
    pub fn scale(scale: Size<Pixels>) -> Self {
        Self {
            scale,
            translate: point(px(0.0), px(0.0)),
            rotate: 0.0,
        }
    }

    /// Create a new Transformation with the specified translation.
    pub fn translate(translate: Point<Pixels>) -> Self {
        Self {
            scale: size(px(1.0), px(1.0)),
            translate,
            rotate: 0.0,
        }
    }

    /// Create a new Transformation with the specified rotation.
    pub fn rotate(rotate: f32) -> Self {
        Self {
            scale: size(px(1.0), px(1.0)),
            translate: point(px(0.0), px(0.0)),
            rotate,
        }
    }

    /// Update the scaling factor of this transformation.
    pub fn with_scaling(mut self, scale: Size<Pixels>) -> Self {
        self.scale = scale;
        self
    }

    /// Update the translation value of this transformation.
    pub fn with_translation(mut self, translate: Point<Pixels>) -> Self {
        self.translate = translate;
        self
    }

    /// Update the rotation angle of this transformation.
    pub fn with_rotation(mut self, rotate: f32) -> Self {
        self.rotate = rotate;
        self
    }

    fn into_matrix(self, size: Size<Pixels>) -> TransformationMatrix {
        TransformationMatrix::unit()
            .translate(self.translate)
            .rotation(self.rotate, size)
            .scale(self.scale, size)
    }
}
