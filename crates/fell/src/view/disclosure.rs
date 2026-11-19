//! Shared layout and fade-through for conversation disclosures.

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, ContentMask, Element, ElementId, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, ParentElement, Pixels, Size, Style, Styled, Window,
    div, px, size,
};

use crate::motion;

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
    let layer = |element, alpha| {
        div()
            .w(px(width))
            .opacity(alpha)
            .child(element)
            .into_any_element()
    };
    Disclosure {
        summary: layer(summary(), summary_alpha),
        details: layer(details(), details_alpha),
        shown,
    }
    .into_any_element()
}

/// A fade-through at a common top edge, so the two sets of glyphs do not
/// overprint one another.
fn disclosure_opacities(shown: f32) -> (f32, f32) {
    ((1.0 - 2.0 * shown).max(0.0), (2.0 * shown - 1.0).max(0.0))
}

/// Layout uses both wrapped heights at the current width. Only the
/// details can be masked, and the summary stays at its own baseline.
struct Disclosure {
    summary: AnyElement,
    details: AnyElement,
    shown: f32,
}

impl IntoElement for Disclosure {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Disclosure {
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
        let closed = self.summary.layout_as_root(space, window, cx).height;
        let open = self.details.layout_as_root(space, window, cx).height;
        let height = motion::lerp(f32::from(closed), f32::from(open), self.shown);
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
        let (summary_alpha, details_alpha) = disclosure_opacities(self.shown);
        if summary_alpha > 0.0 {
            self.summary.prepaint_at(bounds.origin, window, cx);
        }
        if details_alpha > 0.0 {
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                self.details.prepaint_at(bounds.origin, window, cx);
            });
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
        let (summary_alpha, details_alpha) = disclosure_opacities(self.shown);
        if summary_alpha > 0.0 {
            self.summary.paint(window, cx);
        }
        if details_alpha > 0.0 {
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                self.details.paint(window, cx);
            });
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
