//! Photoshop refuses to paint on or move a hidden layer (#571): the painting and retouching
//! tools, the Paint Bucket, the Gradient and the Move tool stop with "the target layer is hidden"
//! instead of changing pixels nobody can see. Filters are refused too (Photoshop greys the Filter
//! menu out), and so are Cut, Copy, Clear and Fill while there is a selection (#10476); without
//! one, Fill fills the whole hidden layer, as Photoshop does. A layer inside a hidden group is
//! hidden too. An alpha channel or Quick Mask target has no layer, so it is never refused.

use photocraft_doc::{Document, LayerId};
use serde_json::Value;

use crate::Session;

/// Photoshop's alert text.
pub const HIDDEN: &str = "Could not complete your request because the target layer is hidden.";

/// Commands that paint on, or move, the target layer.
const GATED: &[&str] = &[
    "paint.stroke",
    "paint.pencil",
    "paint.mixerBrush",
    "paint.colorReplacement",
    "paint.bucket",
    "paint.gradient",
    "paint.magicEraser",
    "paint.backgroundEraser",
    "paint.cloneStamp",
    "paint.healingBrush",
    "paint.spotHealing",
    "paint.patch",
    "paint.dodge",
    "paint.burn",
    "paint.sponge",
    "paint.blur",
    "paint.sharpen",
    "paint.smudge",
    "paint.historyBrush",
    "paint.artHistoryBrush",
    "layer.translate",
];

/// Commands refused on a hidden layer only while there is a selection (#10476).
const GATED_WITH_SELECTION: &[&str] = &[
    "edit.cut",
    "edit.copy",
    "edit.clear",
    "edit.fill",
    "edit.fillForeground",
    "edit.fillBackground",
    "edit.fillForegroundPreserve",
    "edit.fillBackgroundPreserve",
    "edit.fillHistory",
];

/// Does command `id` change the target layer's pixels through a filter. Convert for Smart Filters
/// only wraps the layer, so it is not one.
fn is_filter(id: &str) -> bool {
    id.starts_with("filter.") && id != "filter.convertForSmartFilters"
}

/// Is command `id` refused on a hidden layer, given whether the document has a selection.
fn gated(id: &str, has_selection: bool) -> bool {
    GATED.contains(&id) || is_filter(id) || (has_selection && GATED_WITH_SELECTION.contains(&id))
}

/// Is the layer shown: it and every group around it visible.
fn shown(doc: &Document, id: LayerId) -> bool {
    doc.path_of(id).is_some_and(|path| (1..=path.len()).all(|n| path.get(..n).and_then(|p| doc.layer_at(p)).is_some_and(|l| l.visible)))
}

/// Why command `id` with `params` must not run: its target layer is hidden. `None` for other
/// commands, channel targets, and visible layers.
pub fn refusal(s: &Session, id: &str, params: &Value) -> Option<&'static str> {
    let st = s.active()?;
    if !gated(id, st.doc.selection.is_some()) {
        return None;
    }
    let params = crate::channel_cmds::inject_target(s, id, params.clone());
    if crate::channel_cmds::is_channel_target(&params) {
        return None;
    }
    let layer = params.get("layer").and_then(Value::as_u64).map(LayerId).or(st.active_layer)?;
    (st.doc.layer(layer).is_some() && !shown(&st.doc, layer)).then_some(HIDDEN)
}

/// [`refusal`] for menu enablement: Photoshop greys a filter out on a hidden layer, while the
/// other gated commands stay enabled and are refused when they run.
pub fn menu_refusal(s: &Session, id: &str, params: &Value) -> Option<&'static str> {
    if is_filter(id) { refusal(s, id, params) } else { None }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::HIDDEN;
    use crate::Session;

    fn session() -> (Session, u64) {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 64, "height": 64, "background": "white"})).unwrap();
        let id = s.execute("layer.new.layer", json!({})).unwrap()["layer"].as_u64().unwrap();
        (s, id)
    }

    fn pixel(s: &mut Session) -> serde_json::Value {
        s.execute("document.pixel", json!({"x": 10, "y": 10})).unwrap()
    }

    #[test]
    fn painting_or_moving_a_hidden_layer_is_refused() {
        let (mut s, id) = session();
        s.execute("layer.setProps", json!({"layer": id, "visible": false})).unwrap();
        let steps = s.active().unwrap().history.entries().len();
        for (cmd, p) in [
            ("paint.stroke", json!({"points": [[10, 10], [20, 20]], "color": "#ff0000"})),
            ("paint.bucket", json!({"x": 10, "y": 10})),
            ("paint.gradient", json!({"from": [0, 0], "to": [60, 60]})),
            ("layer.translate", json!({"dx": 5, "dy": 0})),
        ] {
            let e = s.execute(cmd, p).unwrap_err().to_string();
            assert!(e.contains(HIDDEN), "{cmd}: {e}");
        }
        assert_eq!(s.active().unwrap().history.entries().len(), steps, "nothing changed");
        // Shown again, it paints.
        s.execute("layer.setProps", json!({"layer": id, "visible": true})).unwrap();
        s.execute("paint.stroke", json!({"points": [[10, 10], [12, 12]], "color": "#ff0000", "size": 9})).unwrap();
        assert_ne!(pixel(&mut s), json!([1.0, 1.0, 1.0, 1.0]));
    }

    /// #10476: filters are refused and greyed out on a hidden layer; Cut, Copy, Clear and Fill are
    /// refused while there is a selection, and Fill without one fills the whole hidden layer.
    #[test]
    fn filters_and_selection_edits_on_a_hidden_layer_are_refused() {
        let (mut s, id) = session();
        s.execute("edit.fill", json!({"color": [0.0, 1.0, 0.0]})).unwrap();
        s.execute("layer.setProps", json!({"layer": id, "visible": false})).unwrap();
        s.execute("select.all", json!({})).unwrap();
        let steps = s.active().unwrap().history.entries().len();
        for (cmd, p) in [
            ("filter.blur.gaussianBlur", json!({"radius": 2})),
            ("filter.other.minimum", json!({"radius": 1})),
            ("edit.cut", json!({})),
            ("edit.copy", json!({})),
            ("edit.clear", json!({})),
            ("edit.fill", json!({"color": [1.0, 0.0, 0.0]})),
            ("edit.fillForeground", json!({})),
        ] {
            let e = s.execute(cmd, p).unwrap_err().to_string();
            assert!(e.contains(HIDDEN), "{cmd}: {e}");
        }
        assert_eq!(s.active().unwrap().history.entries().len(), steps, "nothing changed");
        assert!(!s.is_enabled("filter.blur.gaussianBlur"), "the filter is greyed out");
        assert!(s.is_enabled("filter.convertForSmartFilters"), "converting only wraps the layer");
        assert!(s.is_enabled("edit.fill"), "Fill stays in the menu and is refused when it runs");
        // Without a selection Fill fills the whole hidden layer, as in Photoshop; filters stay refused.
        s.execute("select.deselect", json!({})).unwrap();
        s.execute("edit.fill", json!({"color": [1.0, 0.0, 0.0]})).unwrap();
        s.execute("layer.setProps", json!({"layer": id, "visible": true})).unwrap();
        assert_eq!(pixel(&mut s), json!([1.0, 0.0, 0.0, 1.0]));
        assert!(s.is_enabled("filter.blur.gaussianBlur"), "shown again, it filters");
    }

    #[test]
    fn a_layer_in_a_hidden_group_is_hidden() {
        let (mut s, id) = session();
        s.execute("layer.groupLayers", json!({})).unwrap();
        let g = s.active().unwrap().active_layer.unwrap().0;
        assert_ne!(g, id, "the new group is active");
        s.execute("layer.setProps", json!({"layer": g, "visible": false})).unwrap();
        s.execute("layer.select", json!({"layer": id})).unwrap();
        assert!(s.execute("paint.stroke", json!({"points": [[10, 10]]})).unwrap_err().to_string().contains(HIDDEN));
    }

    #[test]
    fn alpha_channels_of_a_hidden_layer_still_paint() {
        let (mut s, id) = session();
        s.execute("layer.setProps", json!({"layer": id, "visible": false})).unwrap();
        s.execute("channel.new", json!({})).unwrap();
        s.execute("paint.stroke", json!({"points": [[10, 10], [20, 20]], "target": {"channel": 0}})).unwrap();
    }
}
