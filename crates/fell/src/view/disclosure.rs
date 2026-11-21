//! Shared layout and fade-through for conversation disclosures.

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, ContentMask, Edges, Element, ElementId,
    GlobalElementId, InspectorElementId, IntoElement, LayoutId, ParentElement, Pixels, Size, Style,
    Styled, Window, div, px, size,
};

use crate::motion;

/// Reserve the current height in the thread stack and clip the full card
/// to it while the newly arrived body grows into view.
pub(super) struct GrowingMessage {
    pub inner: AnyElement,
    pub width: Pixels,
    pub from: f32,
    pub progress: f32,
    pub gap: Pixels,
}

impl IntoElement for GrowingMessage {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for GrowingMessage {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let space = size(
            AvailableSpace::Definite(self.width),
            AvailableSpace::MaxContent,
        );
        let full = self.inner.layout_as_root(space, window, cx).height;
        let height = motion::lerp(self.from, f32::from(full), self.progress);
        let style = Style {
            size: Size {
                height: px(motion::snap(height, window.scale_factor())).into(),
                ..Size::auto()
            },
            margin: Edges {
                bottom: self.gap.into(),
                ..Edges::default()
            },
            ..Style::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            self.inner.prepaint_at(bounds.origin, window, cx);
        });
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            self.inner.paint(window, cx);
        });
    }
}

pub(super) fn disclosure(
    width: f32,
    open: bool,
    reveal: Option<f32>,
    summary: impl FnOnce() -> AnyElement,
    details: impl FnOnce() -> AnyElement,
) -> AnyElement {
    let shown = reveal.unwrap_or(if open { 1.0 } else { 0.0 });
    if shown <= 0.0 {
        return summary();
    }
    if shown >= 1.0 {
        return details();
    }

    let (summary_alpha, details_alpha) = disclosure_opacities(shown);
    layered(
        width,
        [summary(), details()],
        [summary_alpha, details_alpha],
        shown,
        [false, true],
    )
}

/// A fade-through at a common top edge, so the two sets of glyphs do not
/// overprint one another.
fn disclosure_opacities(shown: f32) -> (f32, f32) {
    ((1.0 - 2.0 * shown).max(0.0), (2.0 * shown - 1.0).max(0.0))
}

/// Two separately laid-out measures at one origin. The row below sees an
/// interpolated height while each layer keeps its own line breaks.
pub(crate) fn reflow(
    width: f32,
    frame: motion::ReadingReflow,
    layers: [AnyElement; 2],
) -> AnyElement {
    layered(width, layers, frame.alpha, frame.height, [true; 2])
}

/// Reveal a measured child at its natural width while its slot grows or closes.
pub(super) fn reveal(width: f32, height: f32, alpha: f32, child: AnyElement) -> AnyElement {
    if height >= 1.0 {
        return div().opacity(alpha).child(child).into_any_element();
    }
    layered(
        width,
        [div().into_any_element(), child],
        [0.0, alpha],
        height,
        [false, true],
    )
}

/// Keep the child's layout width and mask it at the moving padded edge.
pub(crate) fn clip_column(inset: f32, width: f32, right: f32, child: gpui::Div) -> gpui::Div {
    if inset + width <= right {
        return child.ml(px(inset)).w(px(width));
    }
    div()
        .ml(px(inset))
        .w(px((right - inset).max(0.0)))
        .overflow_hidden()
        .child(child.w(px(width)))
}

fn layered(
    width: f32,
    layers: [AnyElement; 2],
    alpha: [f32; 2],
    height: f32,
    clipped: [bool; 2],
) -> AnyElement {
    let layer = |element, opacity| {
        div()
            .w(px(width))
            .opacity(opacity)
            .child(element)
            .into_any_element()
    };
    let [from, to] = layers;
    Layered {
        layers: [layer(from, alpha[0]), layer(to, alpha[1])],
        alpha,
        height,
        clipped,
    }
    .into_any_element()
}

struct Layered {
    layers: [AnyElement; 2],
    alpha: [f32; 2],
    height: f32,
    clipped: [bool; 2],
}

impl IntoElement for Layered {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Layered {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let space = size(AvailableSpace::MaxContent, AvailableSpace::MaxContent);
        let [from, to] = self
            .layers
            .each_mut()
            .map(|layer| f32::from(layer.layout_as_root(space, window, cx).height));
        let height = motion::lerp(from, to, self.height);
        let style = Style {
            size: Size {
                height: px(height).into(),
                ..Size::auto()
            },
            ..Style::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        for (index, layer) in self.layers.iter_mut().enumerate() {
            if self.alpha[index] > 0.0 {
                window.with_content_mask(
                    self.clipped[index].then_some(ContentMask { bounds }),
                    |window| {
                        layer.prepaint_at(bounds.origin, window, cx);
                    },
                );
            }
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        for (index, layer) in self.layers.iter_mut().enumerate() {
            if self.alpha[index] > 0.0 {
                window.with_content_mask(
                    self.clipped[index].then_some(ContentMask { bounds }),
                    |window| {
                        layer.paint(window, cx);
                    },
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::disclosure_opacities;

    #[test]
    fn a_disclosure_fades_through_before_the_other_layer_appears() {
        assert_eq!(disclosure_opacities(0.0), (1.0, 0.0));
        assert_eq!(disclosure_opacities(0.25), (0.5, 0.0));
        assert_eq!(disclosure_opacities(0.5), (0.0, 0.0));
        assert_eq!(disclosure_opacities(0.75), (0.0, 0.5));
        assert_eq!(disclosure_opacities(1.0), (0.0, 1.0));
    }
}
