//! SVG/SVGZ export: solid shapes as paths, everything else as embedded PNG, checked by reading the
//! file back through the SVG importer (an independent renderer) and comparing the composite.

use std::io::Read;

use photocraft_color::{BlendMode, Color, ColorMode, PixelFormat, SampleType};
use photocraft_doc::{Document, Fill, Layer, LayerContent, Path, PathOp, ShapeLayer, ShapeStroke, Subpath};
use photocraft_geom::{Rect, Size};
use photocraft_raster::Surface;

fn shape(doc: &Document, name: &str, path: Path, color: Color, stroke: Option<ShapeStroke>) -> Layer {
    let mut shape = ShapeLayer { path, fill: Some(Fill::Solid(color)), stroke, ..Default::default() };
    shape.cache = Some(photocraft_vector::render_shape(&shape, PixelFormat::new(doc.mode, doc.depth, true), doc.bounds()));
    Layer::new(name, LayerContent::Shape(shape))
}

/// Pixels (blue top half, translucent red bottom half) under a translucent shape with a hole and a
/// dashed outline.
fn fixture() -> Document {
    let mut doc = Document::new("Mixed", Size::new(64, 48), ColorMode::Rgb, SampleType::U8);
    let mut raster = Surface::new(PixelFormat::new(doc.mode, doc.depth, true));
    raster.fill_rect(Rect::new(0, 0, 64, 24), &[0.15, 0.25, 0.7, 1.0]);
    raster.fill_rect(Rect::new(0, 24, 64, 48), &[0.7, 0.2, 0.15, 0.8]);
    doc.layers.push(Layer::new("Pixels", LayerContent::Raster(raster)));
    let path = Path::new(vec![
        Subpath::polygon(&[(8.0, 8.0), (52.0, 8.0), (52.0, 40.0), (8.0, 40.0)]),
        Subpath::polygon(&[(20.0, 16.0), (20.0, 32.0), (40.0, 32.0), (40.0, 16.0)]).with_op(PathOp::Join),
    ]);
    let stroke = ShapeStroke { width: 3.0, paint: Fill::Solid(Color::rgba(0.9, 0.8, 0.1, 0.7)), opacity: 0.8, dashes: vec![3.0, 2.0], ..Default::default() };
    let mut layer = shape(&doc, "Vector & <stroke>", path, Color::rgba(0.2, 0.8, 0.4, 0.6), Some(stroke));
    layer.opacity = 0.65;
    doc.layers.push(layer);
    doc
}

fn export(doc: &Document, ext: &str) -> photocraft_io::ExportResult {
    photocraft_io::export(doc, ext, &Default::default()).unwrap_or_else(|e| panic!("{ext} export failed: {e}"))
}

/// The exported file, read back, looks like `doc` away from anti-aliased edges.
fn draws_like(doc: &Document, bytes: &[u8], points: &[(usize, usize)]) {
    let name = if bytes.starts_with(&[0x1f, 0x8b]) { "export.svgz" } else { "export.svg" };
    let reopened = photocraft_io::import(name, bytes).expect("the exported SVG opens").document;
    let a = photocraft_compose::flatten(doc).to_rgba8();
    let b = photocraft_compose::flatten(&reopened).to_rgba8();
    assert_eq!((a.width, a.height), (b.width, b.height));
    for &(x, y) in points {
        let at = (y * a.width as usize + x) * 4;
        let (pa, pb) = (&a.pixels[at..at + 4], &b.pixels[at..at + 4]);
        assert!(pa.iter().zip(pb).all(|(p, q)| p.abs_diff(*q) <= 3), "at ({x},{y}): {pa:?} vs {pb:?}");
    }
}

const INSIDE: [(usize, usize); 6] = [(1, 1), (1, 46), (15, 20), (45, 30), (30, 20), (30, 28)];

#[test]
fn solid_shapes_become_paths_over_the_pixels_and_draw_like_the_composite() {
    let mut doc = fixture();
    let mut hidden = doc.layers[1].clone();
    hidden.visible = false;
    hidden.name = "Invisible".into();
    doc.layers.push(hidden);
    let svg = export(&doc, "svg");
    let text = String::from_utf8(svg.bytes.clone()).unwrap();
    assert_eq!(text.matches("<path ").count(), 1, "one path for the visible shape");
    assert_eq!(text.matches("<image ").count(), 1, "one image for the pixel layer");
    assert!(text.contains("stroke-dasharray") && !text.contains("Invisible"));
    draws_like(&doc, &svg.bytes, &INSIDE);
}

#[test]
fn a_layer_that_depends_on_the_layers_under_it_bakes_them_and_says_so() {
    let mut doc = fixture();
    let mut multiply = doc.layers[0].clone();
    multiply.name = "Multiply".into();
    multiply.blend = BlendMode::Multiply;
    multiply.opacity = 0.4;
    doc.layers.push(multiply);
    let top = shape(&doc, "Top", Path::new(vec![Subpath::polygon(&[(3.0, 3.0), (12.0, 3.0), (3.0, 12.0)])]), Color::rgb(0.9, 0.1, 0.2), None);
    doc.layers.push(top);
    let svg = export(&doc, "svg");
    assert!(svg.warnings.iter().any(|w| w.contains("'Multiply'")), "{:?}", svg.warnings);
    let text = String::from_utf8(svg.bytes.clone()).unwrap();
    assert_eq!(text.matches("<path ").count(), 1, "the shape under the multiply layer is baked, the one above is kept");
    assert_eq!(text.matches("<image ").count(), 1);
    draws_like(&doc, &svg.bytes, &[(4, 4), (1, 46), (15, 20), (45, 30), (30, 28)]);
}

#[test]
fn documents_svg_cannot_paint_exactly_stay_pixels() {
    let mut linear = fixture();
    linear.icc_profile = Some(craft_cms::Builtin::LinearSrgb.profile().to_bytes());
    let mut boolean = fixture();
    if let Some(LayerContent::Shape(shape)) = boolean.layers.get_mut(1).map(|l| &mut l.content) {
        shape.path.subpaths[1].op = PathOp::Subtract;
    }
    for doc in [linear, boolean] {
        let svg = export(&doc, "svg");
        let text = String::from_utf8(svg.bytes.clone()).unwrap();
        assert!(!text.contains("<path ") && text.matches("<image ").count() == 1);
        assert!(svg.warnings.iter().any(|w| w.contains("one raster image")), "{:?}", svg.warnings);
    }
}

#[test]
fn svgz_is_the_same_svg_gzipped() {
    let doc = fixture();
    let svg = export(&doc, "svg");
    let svgz = export(&doc, "drawing.svgz");
    let mut unzipped = Vec::new();
    flate2::read::GzDecoder::new(svgz.bytes.as_slice()).read_to_end(&mut unzipped).expect("SVGZ is gzip");
    assert_eq!(unzipped, svg.bytes);
    draws_like(&doc, &svgz.bytes, &INSIDE);
}
