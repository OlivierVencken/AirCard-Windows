use anyhow::{Context, Result, ensure};
use quick_xml::{Reader, events::Event};
use svgtypes::{PathParser, PathSegment, SimplePathSegment as Segment, SimplifyingPathParser};

pub(super) const MAX_SVG_BYTES: usize = 256 * 1024;
const MAX_PATH_SEGMENTS: usize = 4096;
const MAX_PATH_NUMBER: f64 = 16.0 * 8192.0;

pub(super) struct Shape {
    /// Normalized outline for inspection.
    pub contours: Vec<Vec<[f32; 2]>>,
}

pub(super) fn validate(bytes: &[u8]) -> Result<Shape> {
    ensure!(
        bytes.len() <= MAX_SVG_BYTES,
        "Transaction SVG exceeds 256 KiB"
    );
    let text = std::str::from_utf8(bytes).context("Transaction SVG must be UTF-8")?;
    // No entity expansion, linked resources, CSS, transforms or executable content.
    ensure!(
        !text.contains('&'),
        "Transaction SVG must not contain entities"
    );
    let mut reader = Reader::from_str(text);
    let mut depth = 0;
    let mut root_seen = false;
    let mut path_seen = false;
    let mut width = None;
    let mut height = None;
    let mut view_box = None;
    let mut path = None;
    let mut declaration_seen = false;
    loop {
        let event = reader.read_event().context("Invalid transaction SVG XML")?;
        let empty = matches!(event, Event::Empty(_));
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                if depth == 0 && !root_seen && e.name().as_ref() == "svg" {
                    root_seen = true;
                    for attribute in e.attributes() {
                        let a = attribute?;
                        let value = a.value.as_ref();
                        match a.key.as_ref() {
                            "xmlns" => ensure!(
                                value == "http://www.w3.org/2000/svg",
                                "Invalid SVG namespace"
                            ),
                            "width" => width = Some(dimension(value)?),
                            "height" => height = Some(dimension(value)?),
                            "viewBox" => view_box = Some(value.to_string()),
                            "version" => {
                                ensure!(matches!(value, "1.0" | "1.1"), "Unsupported SVG version")
                            }
                            _ => anyhow::bail!("Transaction SVG has unsupported root attributes"),
                        }
                    }
                } else {
                    ensure!(
                        depth == 1 && !path_seen && e.name().as_ref() == "path",
                        "Use one direct path child in the transaction SVG"
                    );
                    path_seen = true;
                    for attribute in e.attributes() {
                        let a = attribute?;
                        ensure!(
                            a.key.as_ref() == "d",
                            "Transaction SVG path supports only the d attribute"
                        );
                        path = Some(a.value.as_ref().to_string());
                    }
                }
                if !empty {
                    depth += 1;
                }
            }
            Event::End(_) => {
                ensure!(depth > 0, "Invalid transaction SVG structure");
                depth -= 1;
            }
            Event::Text(e) => ensure!(
                e.as_ref().bytes().all(|b| b.is_ascii_whitespace()),
                "Transaction SVG must contain only a path"
            ),
            Event::Decl(declaration) if !root_seen && !declaration_seen => {
                ensure!(
                    declaration.version()?.as_ref() == "1.0",
                    "Invalid transaction SVG XML"
                );
                if let Some(encoding) = declaration.encoding() {
                    ensure!(
                        encoding?.eq_ignore_ascii_case("utf-8"),
                        "Transaction SVG must be UTF-8"
                    );
                }
                declaration_seen = true;
            }
            Event::Eof => break,
            _ => anyhow::bail!("Transaction SVG must contain only a root and one path"),
        }
    }
    ensure!(
        root_seen && path_seen && depth == 0,
        "Use one direct path child in the transaction SVG"
    );
    let width = width.context("Transaction SVG needs positive width and height in pixels")?;
    let height = height.context("Transaction SVG needs positive width and height in pixels")?;
    if let Some(view_box) = view_box {
        let values: Vec<f64> = view_box
            .split(|c: char| c.is_ascii_whitespace() || c == ',')
            .filter(|s| !s.is_empty())
            .map(str::parse)
            .collect::<std::result::Result<_, _>>()?;
        ensure!(
            values == [0.0, 0.0, width, height],
            "SVG viewBox must be 0 0 width height"
        );
    }
    let path = path.context("Transaction SVG needs path data")?;
    validate_path_numbers(&path)?;
    let mut contours: Vec<Vec<[f32; 2]>> = Vec::new();
    let mut current = [0.0; 2];
    for (index, segment) in SimplifyingPathParser::from(path.as_str()).enumerate() {
        ensure!(
            index < MAX_PATH_SEGMENTS,
            "Transaction SVG has too many path segments"
        );
        let segment = segment.context("Invalid transaction SVG path")?;
        let point = |x: f64, y: f64| -> Result<[f64; 2]> {
            let p = [x / width, y / height];
            ensure!(
                p.iter().all(|v| v.is_finite() && v.abs() <= 16.0),
                "Transaction SVG coordinates are out of range"
            );
            Ok(p)
        };
        let mut samples = Vec::new();
        match segment {
            Segment::MoveTo { x, y } => {
                current = point(x, y)?;
                contours.push(vec![[current[0] as f32, current[1] as f32]]);
            }
            Segment::LineTo { x, y } => samples.push(point(x, y)?),
            Segment::CurveTo {
                x1,
                y1,
                x2,
                y2,
                x,
                y,
            } => {
                let a = point(x1, y1)?;
                let b = point(x2, y2)?;
                let c = point(x, y)?;
                for i in 1..=24 {
                    let t = i as f64 / 24.0;
                    let u = 1.0 - t;
                    samples.push(std::array::from_fn(|j| {
                        u * u * u * current[j]
                            + 3.0 * u * u * t * a[j]
                            + 3.0 * u * t * t * b[j]
                            + t * t * t * c[j]
                    }));
                }
            }
            Segment::Quadratic { x1, y1, x, y } => {
                let a = point(x1, y1)?;
                let b = point(x, y)?;
                for i in 1..=24 {
                    let t = i as f64 / 24.0;
                    let u = 1.0 - t;
                    samples.push(std::array::from_fn(|j| {
                        u * u * current[j] + 2.0 * u * t * a[j] + t * t * b[j]
                    }));
                }
            }
            Segment::ClosePath => {
                if let Some(first) = contours.last().and_then(|c| c.first()) {
                    samples.push([first[0] as f64, first[1] as f64]);
                }
            }
        }
        if let Some(last) = samples.last() {
            current = *last;
        }
        if !samples.is_empty() {
            contours
                .last_mut()
                .context("SVG path must start with a move command")?
                .extend(samples.into_iter().map(|p| [p[0] as f32, p[1] as f32]));
        }
    }
    ensure!(
        contours.iter().any(|c| c
            .windows(2)
            .any(|p| (p[0][0] - p[1][0]).abs() + (p[0][1] - p[1][1]).abs() > 0.00001)),
        "Transaction SVG has no visible outline"
    );
    ensure!(
        contours
            .iter()
            .any(|c| c.windows(2).any(|p| visible_segment(p[0], p[1]))),
        "Transaction SVG outline is outside its canvas"
    );
    Ok(Shape { contours })
}

/// Clip sampled outline edges against the unit canvas. An isolated move inside
/// the canvas cannot make a different, entirely off-canvas contour visible.
fn visible_segment(a: [f32; 2], b: [f32; 2]) -> bool {
    let mut enter = 0.0_f32;
    let mut leave = 1.0_f32;
    for axis in 0..2 {
        let delta = b[axis] - a[axis];
        if delta.abs() < f32::EPSILON {
            if !(0.0..=1.0).contains(&a[axis]) {
                return false;
            }
        } else {
            let first = -a[axis] / delta;
            let last = (1.0 - a[axis]) / delta;
            enter = enter.max(first.min(last));
            leave = leave.min(first.max(last));
        }
    }
    leave - enter > f32::EPSILON && a != b
}

/// Reject extreme arc radii/rotations before the simplifying parser performs
/// geometric arithmetic. Normalized output coordinates are checked separately.
fn validate_path_numbers(path: &str) -> Result<()> {
    for (index, segment) in PathParser::from(path).enumerate() {
        ensure!(
            index < MAX_PATH_SEGMENTS,
            "Transaction SVG has too many path segments"
        );
        let values = match segment.context("Invalid transaction SVG path")? {
            PathSegment::MoveTo { x, y, .. }
            | PathSegment::LineTo { x, y, .. }
            | PathSegment::SmoothQuadratic { x, y, .. } => [x, y, 0.0, 0.0, 0.0, 0.0],
            PathSegment::HorizontalLineTo { x, .. } => [x, 0.0, 0.0, 0.0, 0.0, 0.0],
            PathSegment::VerticalLineTo { y, .. } => [y, 0.0, 0.0, 0.0, 0.0, 0.0],
            PathSegment::CurveTo {
                x1,
                y1,
                x2,
                y2,
                x,
                y,
                ..
            } => [x1, y1, x2, y2, x, y],
            PathSegment::SmoothCurveTo { x2, y2, x, y, .. } => [x2, y2, x, y, 0.0, 0.0],
            PathSegment::Quadratic { x1, y1, x, y, .. } => [x1, y1, x, y, 0.0, 0.0],
            PathSegment::EllipticalArc {
                rx,
                ry,
                x_axis_rotation,
                x,
                y,
                ..
            } => [rx, ry, x_axis_rotation, x, y, 0.0],
            PathSegment::ClosePath { .. } => [0.0; 6],
        };
        ensure!(
            values
                .iter()
                .all(|v| v.is_finite() && v.abs() <= MAX_PATH_NUMBER),
            "Transaction SVG coordinates are out of range"
        );
    }
    Ok(())
}

fn dimension(value: &str) -> Result<f64> {
    let value: f64 = value.strip_suffix("px").unwrap_or(value).parse()?;
    ensure!(
        value.is_finite() && (0.001..=8192.0).contains(&value),
        "Transaction SVG needs positive width and height in pixels"
    );
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    pub const SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100" viewBox="0 0 100 100"><path d="M 50 5 L 95 95 L 5 95 Z"/></svg>"#;
    #[test]
    fn rejects_unsafe_or_unusable_shapes() {
        assert_eq!(validate(SVG).unwrap().contours[0].len(), 4);
        for invalid in [
            "<svg width='0' height='100'><path d='M0 0L1 1'/></svg>",
            "<svg width='100' height='100'><g><path d='M0 0L1 1'/></g></svg>",
            "<svg width='100' height='100'><path d='M0 0'/></svg>",
            "<svg width='100' height='100'><path d='garbage'/></svg>",
            "<svg width='100' height='100'><path d='M0 0L1 1' transform='scale(2)'/></svg>",
            "<!DOCTYPE svg><svg width='100' height='100'><path d='M0 0L1 1'/></svg>",
            "<svg width='100' height='100'><path d='M0 0L1e300 1'/></svg>",
            "<svg width='100' height='100'><path d='M0 0L1 1'/><path d='M0 0L1 1'/></svg>",
            "<svg width='100' height='100'><image href='https://example.org/a'/></svg>",
        ] {
            assert!(validate(invalid.as_bytes()).is_err(), "{invalid}");
        }
        assert!(validate(&vec![b' '; MAX_SVG_BYTES + 1]).is_err());
    }

    #[test]
    fn bounds_curve_arithmetic_and_rejects_ambiguous_xml() {
        let svg = |path: &str| format!("<svg width='100' height='100'><path d='{path}'/></svg>");
        for path in [
            "M10 10A1e300 1e300 0 0 0 90 90",
            "M10 10A10 10 1e300 0 0 90 90",
            "M10 10C1e300 0 0 1 90 90",
            "M10 10l1e300 1",
        ] {
            assert!(validate(svg(path).as_bytes()).is_err(), "{path}");
        }
        let complex = format!("M0 0{}", "L10 10 ".repeat(MAX_PATH_SEGMENTS));
        assert!(validate(svg(&complex).as_bytes()).is_err());
        assert!(validate(svg("M50 50M1000 1000L1100 1100").as_bytes()).is_err());
        assert!(validate(svg("M-50 50L150 50").as_bytes()).is_ok());
        for document in [
            "<svg width='100' height='100'><path d='M0 0L10 10'></svg>",
            "<svg width='100' width='200' height='100'><path d='M0 0L10 10'/></svg>",
            "<svg width='100' height='100'><path d='M0 0L10 10' d='M20 20L30 30'/></svg>",
            "<?xml version='1.0'?><?xml version='1.0'?><svg width='100' height='100'><path d='M0 0L10 10'/></svg>",
            "<?xml version='1.0' encoding='UTF-16'?><svg width='100' height='100'><path d='M0 0L10 10'/></svg>",
        ] {
            assert!(validate(document.as_bytes()).is_err(), "{document}");
        }
        for path in [
            "M10 50A40 40 0 1 1 90 50Z",
            "m10 10h20v20q5 10 10 0t10 0c5 5 5 5 10 0s10 0 10 10z",
        ] {
            assert!(validate(svg(path).as_bytes()).is_ok(), "{path}");
        }
        assert!(
            validate(
                format!(
                    "<?xml version='1.0' encoding='UTF-8'?>{}",
                    svg("M0 0L10 10")
                )
                .as_bytes()
            )
            .is_ok()
        );
    }
}
