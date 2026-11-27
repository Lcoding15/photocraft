//! SVG and SVGZ export (#2872, #3169).
//!
//! Solid shapes (and solid colour fill layers) in a plain sRGB stack become SVG paths; every other
//! layer is kept as PNG pixels, embedded as `<image>` elements. A layer whose look depends on what
//! lies under it (masks, clipping, effects, blend modes other than Normal, text, adjustments, …)
//! bakes itself together with the stack below it into one image, never on its own, so the result
//! looks like the composite. When no layer can be a path, the file holds one image of the whole
//! composite.
//!
//! This is a delivery format: layers, text and effects are not kept as such, and PhotoCraft does
//! not read the shapes back as shape layers. Each loss is reported as a warning.
use std::io::Write;

use base64::Engine;
use craft_cms::Builtin;
use photocraft_codecs::{ChannelLayout, Format, Limits, SampleType};
use photocraft_color::{BlendMode, Color, ColorMode};
use photocraft_doc::vector::{FillRule, LineCap, LineJoin, PathOp, StrokeAlign};
use photocraft_doc::{Document, Fill, Layer, LayerContent, Path, ShapeStroke, Subpath};

use crate::{ExportOptions, ExportResult, IoError};

/// Largest SVG text written (the base64 images included).
const MAX_XML: usize = 128 << 20;
/// Largest total of embedded PNG bytes and path text: base64 grows the images by a third, which
/// keeps the file under [`MAX_XML`].
const MAX_PAYLOAD: usize = 96 << 20;
/// Largest path text for one shape.
const MAX_GEOMETRY: usize = 4 << 20;
/// Most layers looked at once groups are opened.
const MAX_NODES: usize = 4096;
/// Most knots in one shape, over all its subpaths.
const MAX_KNOTS: usize = 16384;
/// Largest coordinate a shape may use, in pixels.
const MAX_COORD: f64 = 1_000_000.0;
/// Most dashes a dashed outline may draw.
const MAX_DASHES_DRAWN: f64 = 50_000.0;

fn error(message: impl std::fmt::Display) -> IoError {
    IoError::Unsupported(format!("SVG export: {message}"))
}

/// One shape, as the SVG draws it.
struct Vector {
    path: Path,
    fill: Option<Color>,
    stroke: Option<(ShapeStroke, Color)>,
    opacity: f32,
}

enum Node {
    Vector { vector: Box<Vector>, d: String },
    Raster { png: Vec<u8>, opacity: f32 },
}

impl Node {
    fn cost(&self) -> usize {
        match self {
            Node::Vector { d, .. } => d.len(),
            Node::Raster { png, .. } => png.len(),
        }
    }
}

/// Exports `doc` as SVG, or as gzipped SVG when `gzip` is set.
pub(crate) fn export(doc: &Document, gzip: bool, options: &ExportOptions) -> Result<ExportResult, IoError> {
    let (w, h) = (doc.size.width, doc.size.height);
    Limits { max_alloc: 256 << 20, ..Default::default() }.check(w, h, ChannelLayout::Rgba, SampleType::U16)?;
    let mut warnings = Vec::new();
    let nodes = match scene(doc, options, &mut warnings)? {
        Some(nodes) => nodes,
        None => {
            warnings.push("SVG output embeds one raster image; layers, paths and text are not kept as such".into());
            vec![Node::Raster { png: png(doc, None, options, &mut warnings)?, opacity: 1.0 }]
        }
    };
    let mut used = 0usize;
    for node in &nodes {
        used = used.checked_add(node.cost()).filter(|used| *used <= MAX_PAYLOAD).ok_or_else(|| error("the images and paths exceed 96 MiB"))?;
    }
    if nodes.iter().all(|node| matches!(node, Node::Vector { .. })) && doc.metadata != Default::default() {
        warnings.push("Document metadata is not written to SVG".into());
    }
    let source = svg(w, h, &nodes)?;
    let bytes = if gzip {
        let mut z = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(source.as_bytes()).map_err(error)?;
        z.finish().map_err(error)?
    } else {
        source.into_bytes()
    };
    Ok(ExportResult { bytes, warnings })
}

/// The paths and images that draw `doc`, bottom first, or `None` when no layer can be a path
/// (or the document is not plain sRGB, whose paint SVG could not reproduce).
fn scene(doc: &Document, options: &ExportOptions, warnings: &mut Vec<String>) -> Result<Option<Vec<Node>>, IoError> {
    // SVG paints in sRGB. Converting each operand of a composite made in another space is not
    // the same as converting the composite, so such a document stays pixels.
    let srgb = Builtin::Srgb.profile().to_bytes();
    if doc.mode != ColorMode::Rgb || doc.icc_profile.as_ref().is_some_and(|p| p.as_slice() != srgb.as_slice()) {
        return Ok(None);
    }
    let mut layers = Vec::new();
    flatten_groups(&doc.layers, &mut layers, 0)?;
    if !layers.iter().any(|layer| vector(layer, doc).is_some()) {
        return Ok(None);
    }
    let mut nodes = Vec::new();
    // The layers up to this index must be baked into one image under the next node.
    let mut pending: Option<usize> = None;
    for (i, layer) in layers.iter().enumerate() {
        let shape = vector(layer, doc);
        let bitmap = plain(layer) && layer.blend == BlendMode::Normal && matches!(layer.content, LayerContent::Raster(_));
        if shape.is_none() && !bitmap {
            pending = Some(i.saturating_add(1));
            warnings.push(format!(
                "Layer '{}' and the layers under it were turned into one image to keep their look",
                layer.name.chars().take(256).collect::<String>()
            ));
            continue;
        }
        if let Some(end) = pending.take() {
            nodes.clear();
            let below = layers.get(..end).ok_or_else(|| error("invalid layer range"))?;
            nodes.push(Node::Raster { png: png(doc, Some(below), options, warnings)?, opacity: 1.0 });
        }
        match shape {
            Some(vector) => {
                let d = path_data(&vector.path);
                if d.len() > MAX_GEOMETRY {
                    return Err(error("a shape's path exceeds 4 MiB"));
                }
                nodes.push(Node::Vector { vector: Box::new(vector), d });
            }
            None => {
                let mut opaque = (*layer).clone();
                opaque.opacity = 1.0;
                opaque.fill_opacity = 1.0;
                let png = png(doc, Some(&[&opaque]), options, warnings)?;
                nodes.push(Node::Raster { png, opacity: layer.opacity * layer.fill_opacity });
            }
        }
    }
    if let Some(end) = pending {
        nodes.clear();
        let below = layers.get(..end).ok_or_else(|| error("invalid layer range"))?;
        nodes.push(Node::Raster { png: png(doc, Some(below), options, warnings)?, opacity: 1.0 });
    }
    Ok(Some(nodes))
}

/// A layer drawn without anything that depends on the layers under it or on its own pixels'
/// surroundings: no mask, clipping, effects, channel or blend-if restrictions, frame or video.
fn plain(layer: &Layer) -> bool {
    layer.mask.is_none()
        && layer.vector_mask.is_none()
        && !layer.clipped
        && layer.frame.is_none()
        && layer.excluded_channels == 0
        && layer.blend_if.is_default()
        && layer.advanced.is_default()
        && layer.video.is_none()
        && (!layer.effects.enabled || (layer.effects.items.is_empty() && layer.effects.psd_raw.is_none()))
        && unit(layer.opacity)
        && unit(layer.fill_opacity)
}

fn unit(v: f32) -> bool {
    v.is_finite() && (0.0..=1.0).contains(&v)
}

/// A solid RGB colour SVG can paint as it is.
fn paint(fill: &Fill) -> Option<Color> {
    let Fill::Solid(c) = fill else { return None };
    (c.mode == ColorMode::Rgb && c.c.iter().all(|v| unit(*v)) && unit(c.alpha)).then_some(*c)
}

/// The shape `layer` draws, when SVG can draw it exactly: a solid shape layer, or a solid colour
/// fill layer (which covers the canvas).
fn vector(layer: &Layer, doc: &Document) -> Option<Vector> {
    if !plain(layer) || layer.blend != BlendMode::Normal || layer.fill_cache.is_some() {
        return None;
    }
    let (path, fill, stroke) = match &layer.content {
        LayerContent::Shape(shape) => (shape.path.clone(), shape.fill.as_ref(), shape.stroke.as_ref()),
        LayerContent::Fill(fill @ Fill::Solid(_)) => {
            let (w, h) = (f64::from(doc.size.width), f64::from(doc.size.height));
            (Path { subpaths: vec![Subpath::polygon(&[(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)])], ..Default::default() }, Some(fill), None)
        }
        _ => return None,
    };
    if !drawable(&path) {
        return None;
    }
    let fill = match fill {
        Some(fill) => Some(paint(fill)?),
        None => None,
    };
    let stroke = match stroke {
        Some(s) if s.width > 0.0 => {
            if s.align != StrokeAlign::Center || !valid_stroke(s) || !bounded_dashes(&path, s) {
                return None;
            }
            Some((s.clone(), paint(&s.paint)?))
        }
        _ => None,
    };
    Some(Vector { path, fill, stroke, opacity: layer.opacity * layer.fill_opacity })
}

/// A path SVG draws as the layer does: not inverted, subpaths only joined (SVG has no path
/// booleans), and its size and coordinates bounded.
fn drawable(path: &Path) -> bool {
    !path.inverted
        && path.subpaths.len() <= MAX_KNOTS
        && path.subpaths.iter().enumerate().all(|(i, p)| {
            (i == 0 || p.op == PathOp::Join)
                && p.knots.len() <= MAX_KNOTS
                && p.knots
                    .iter()
                    .all(|k| [k.anchor, k.in_ctrl, k.out_ctrl].iter().all(|p| p.x.is_finite() && p.y.is_finite() && p.x.abs().max(p.y.abs()) <= MAX_COORD))
        })
        && path.subpaths.iter().map(|p| p.knots.len()).sum::<usize>() <= MAX_KNOTS
}

fn valid_stroke(s: &ShapeStroke) -> bool {
    s.width.is_finite()
        && s.width <= MAX_COORD as f32
        && unit(s.opacity)
        && s.miter_limit.is_finite()
        && (1.0..=500.0).contains(&s.miter_limit)
        && s.dash_offset.is_finite()
        && f64::from(s.dash_offset.abs()) <= MAX_COORD
        && s.dashes.len() <= 32
        && s.dashes.iter().all(|d| d.is_finite() && (0.0..=10000.0).contains(d))
        && (s.dashes.is_empty() || s.dashes.iter().sum::<f32>() >= 0.01)
}

/// Whether a dashed outline draws a bounded number of dashes. Each cubic is no longer than its
/// control polygon, so the polygons' length bounds the path's.
fn bounded_dashes(path: &Path, stroke: &ShapeStroke) -> bool {
    if stroke.dashes.is_empty() {
        return true;
    }
    let period = f64::from(stroke.width) * stroke.dashes.iter().map(|v| f64::from(*v)).sum::<f64>();
    if period <= 1e-9 {
        return false;
    }
    let length = path
        .subpaths
        .iter()
        .flat_map(Subpath::segments)
        .map(|segment| segment.windows(2).map(|pair| if let [a, b] = pair { (b.x - a.x).hypot(b.y - a.y) } else { 0.0 }).sum::<f64>())
        .sum::<f64>();
    length / period * stroke.dashes.len() as f64 <= MAX_DASHES_DRAWN
}

/// Opens the visible groups whose layers can be drawn one by one with the same result (Normal
/// or Pass Through, fully opaque, nothing applied to the group itself); other groups stay one
/// layer, which the scene bakes. Hidden layers are left out.
fn flatten_groups<'a>(layers: &'a [Layer], out: &mut Vec<&'a Layer>, depth: usize) -> Result<(), IoError> {
    if depth > photocraft_doc::MAX_GROUP_DEPTH || out.len().saturating_add(layers.len()) > MAX_NODES {
        return Err(error("the layer tree is too large to export"));
    }
    for layer in layers.iter().filter(|layer| layer.visible) {
        match &layer.content {
            LayerContent::Group(g) if openable(layer) && group_safe(&g.children, depth.saturating_add(1)) => {
                flatten_groups(&g.children, out, depth.saturating_add(1))?;
            }
            _ => out.push(layer),
        }
        if out.len() > MAX_NODES {
            return Err(error("the document has too many layers to export"));
        }
    }
    Ok(())
}

/// A group that changes nothing about how its layers are drawn.
fn openable(layer: &Layer) -> bool {
    matches!(&layer.content, LayerContent::Group(g) if g.artboard.is_none())
        && plain(layer)
        && layer.opacity == 1.0
        && layer.fill_opacity == 1.0
        && matches!(layer.blend, BlendMode::Normal | BlendMode::PassThrough)
}

/// Whether every visible layer in a group draws the same once the group is opened: no
/// adjustment (it would reach below the group) and Normal blending throughout.
fn group_safe(layers: &[Layer], depth: usize) -> bool {
    depth <= photocraft_doc::MAX_GROUP_DEPTH
        && layers.len() <= MAX_NODES
        && layers.iter().all(|l| {
            !l.visible
                || (plain(l)
                    && match &l.content {
                        LayerContent::Group(g) => openable(l) && group_safe(&g.children, depth.saturating_add(1)),
                        LayerContent::Adjustment(_) => false,
                        _ => l.blend == BlendMode::Normal,
                    })
        })
}

/// `doc`, or only `layers` of it, as PNG; its warnings join `warnings` once each.
fn png(doc: &Document, layers: Option<&[&Layer]>, options: &ExportOptions, warnings: &mut Vec<String>) -> Result<Vec<u8>, IoError> {
    let result = match layers {
        Some(layers) => {
            let mut subset = doc.clone();
            subset.layers = layers.iter().map(|layer| (*layer).clone()).collect();
            crate::flat::export_flat(&subset, Format::Png, options)?
        }
        None => crate::flat::export_flat(doc, Format::Png, options)?,
    };
    for warning in result.warnings {
        if !warnings.contains(&warning) {
            warnings.push(warning);
        }
    }
    Ok(result.bytes)
}

/// SVG path data for `path`: every subpath as cubic segments.
fn path_data(path: &Path) -> String {
    let mut out = String::new();
    for subpath in &path.subpaths {
        let Some(first) = subpath.knots.first() else { continue };
        out.push_str(&format!("M {} {} ", first.anchor.x, first.anchor.y));
        for [_, a, b, c] in subpath.segments() {
            out.push_str(&format!("C {} {} {} {} {} {} ", a.x, a.y, b.x, b.y, c.x, c.y));
        }
        if subpath.closed {
            out.push_str("Z ");
        }
    }
    out
}

fn rgb(color: Color) -> String {
    let [r, g, b] = color.to_rgb();
    format!("rgb({}%,{}%,{}%)", r * 100.0, g * 100.0, b * 100.0)
}

fn path_attrs(v: &Vector, d: &str) -> String {
    let mut attrs = format!(
        "d=\"{d}\" fill=\"{}\" fill-opacity=\"{}\" fill-rule=\"{}\"",
        v.fill.map_or_else(|| "none".into(), rgb),
        v.fill.map_or(1.0, |c| c.alpha),
        if v.path.fill_rule == FillRule::EvenOdd { "evenodd" } else { "nonzero" }
    );
    if let Some((s, color)) = &v.stroke {
        let cap = match s.cap {
            LineCap::Butt => "butt",
            LineCap::Round => "round",
            LineCap::Square => "square",
        };
        let join = match s.join {
            LineJoin::Miter => "miter",
            LineJoin::Round => "round",
            LineJoin::Bevel => "bevel",
        };
        // Dashes and their offset are stored in multiples of the width; SVG takes pixels.
        let dash = if s.dashes.is_empty() { "none".into() } else { s.dashes.iter().map(|d| (d * s.width).to_string()).collect::<Vec<_>>().join(" ") };
        attrs.push_str(&format!(
            " stroke=\"{}\" stroke-opacity=\"{}\" stroke-width=\"{}\" stroke-linecap=\"{cap}\" stroke-linejoin=\"{join}\" stroke-miterlimit=\"{}\" stroke-dasharray=\"{dash}\" stroke-dashoffset=\"{}\"",
            rgb(*color),
            color.alpha * s.opacity,
            s.width,
            s.miter_limit,
            s.dash_offset * s.width
        ));
    }
    attrs
}

fn svg(w: u32, h: u32, nodes: &[Node]) -> Result<String, IoError> {
    let mut out = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" overflow=\"hidden\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\">"
    );
    for node in nodes {
        match node {
            Node::Raster { png, opacity } => out.push_str(&format!(
                "<image width=\"{w}\" height=\"{h}\" opacity=\"{opacity}\" xlink:href=\"data:image/png;base64,{}\"/>",
                base64::engine::general_purpose::STANDARD.encode(png)
            )),
            Node::Vector { vector, d } => out.push_str(&format!("<g opacity=\"{}\"><path {}/></g>", vector.opacity, path_attrs(vector, d))),
        }
        if out.len() > MAX_XML {
            return Err(error("the SVG exceeds 128 MiB"));
        }
    }
    out.push_str("</svg>");
    Ok(out)
}
