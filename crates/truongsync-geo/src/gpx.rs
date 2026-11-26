use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration, NaiveDateTime, TimeZone, Utc};
use quick_xml::events::Event;
use quick_xml::Reader;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::Path;
use truongsync_db::models::is_mac_metadata_path;
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TrackPoint {
    pub lat: f64,
    pub lon: f64,
    pub ele: Option<f64>,
    pub time_utc: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackSegment {
    pub points: Vec<TrackPoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedGpx {
    pub files: Vec<String>,
    pub segments: Vec<TrackSegment>,
    pub point_count: usize,
    pub start_utc: Option<DateTime<Utc>>,
    pub end_utc: Option<DateTime<Utc>>,
    /// Downsampled `[lon, lat]` pairs per segment for fast MapLibre rendering.
    pub preview_lines: Vec<Vec<[f64; 2]>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchedGpxMatch {
    pub path: String,
    pub filename: String,
    pub point_count: usize,
    pub start_utc: String,
    pub end_utc: String,
    pub overlap_seconds: i64,
}

/// Parses a single `.gpx` file into time-ordered track segments.
pub fn parse_gpx_file(path: &Path) -> Result<Vec<TrackSegment>> {
    let xml = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read GPX file {}", path.display()))?;
    parse_gpx_str(&xml).with_context(|| format!("failed to parse GPX XML {}", path.display()))
}

fn flush_segment(segments: &mut Vec<TrackSegment>, current_seg: &mut Vec<TrackPoint>) {
    if !current_seg.is_empty() {
        current_seg.sort_by_key(|p| p.time_utc);
        segments.push(TrackSegment {
            points: std::mem::take(current_seg),
        });
    }
}

pub fn parse_gpx_str(xml: &str) -> Result<Vec<TrackSegment>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut segments: Vec<TrackSegment> = Vec::new();
    let mut current_seg: Vec<TrackPoint> = Vec::new();

    let mut cur_lat: Option<f64> = None;
    let mut cur_lon: Option<f64> = None;
    let mut cur_ele: Option<f64> = None;
    let mut cur_time: Option<DateTime<Utc>> = None;
    let mut in_pt = false;
    // Depth relative to `<trkpt>`/`<rtept>`: 0 = inside point, 1 = direct child (`<ele>`, `<time>`), >=2 = nested (`<extensions>`)
    let mut pt_child_depth: usize = 0;
    let mut cur_tag: Option<String> = None;
    let mut text_acc = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let ln = e.local_name();
                let name: &str = ln.as_ref();
                if in_pt {
                    pt_child_depth += 1;
                    if pt_child_depth == 1 && (name == "ele" || name == "time") {
                        cur_tag = Some(name.to_string());
                        text_acc.clear();
                    }
                } else {
                    match name {
                        "trkseg" | "rte" => {
                            flush_segment(&mut segments, &mut current_seg);
                        }
                        "trkpt" | "rtept" => {
                            in_pt = true;
                            pt_child_depth = 0;
                            cur_lat = None;
                            cur_lon = None;
                            cur_ele = None;
                            cur_time = None;
                            cur_tag = None;
                            for attr in e.attributes().flatten() {
                                let kn = attr.key.local_name();
                                let k: &str = kn.as_ref();
                                let v: &str = attr.value.as_ref();
                                if k == "lat" {
                                    cur_lat =
                                        v.trim().parse::<f64>().ok().filter(|x| x.is_finite());
                                } else if k == "lon" {
                                    cur_lon =
                                        v.trim().parse::<f64>().ok().filter(|x| x.is_finite());
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Ok(Event::Text(e)) if in_pt && pt_child_depth == 1 && cur_tag.is_some() => {
                let txt: &str = e.as_ref();
                text_acc.push_str(txt);
            }
            Ok(Event::CData(e)) if in_pt && pt_child_depth == 1 && cur_tag.is_some() => {
                let txt: &str = e.as_ref();
                text_acc.push_str(txt);
            }
            Ok(Event::GeneralRef(e)) if in_pt && pt_child_depth == 1 && cur_tag.is_some() => {
                if let Ok(Some(ch)) = e.resolve_char_ref() {
                    text_acc.push(ch);
                } else if let Some(ent) = quick_xml::escape::resolve_xml_entity(e.as_ref()) {
                    text_acc.push_str(ent);
                }
            }
            Ok(Event::End(ref e)) => {
                let ln = e.local_name();
                let name: &str = ln.as_ref();
                if in_pt {
                    if pt_child_depth == 0 && (name == "trkpt" || name == "rtept") {
                        if let (Some(lat), Some(lon), Some(time_utc)) = (cur_lat, cur_lon, cur_time)
                        {
                            if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) {
                                current_seg.push(TrackPoint {
                                    lat,
                                    lon,
                                    ele: cur_ele,
                                    time_utc,
                                });
                            }
                        }
                        in_pt = false;
                        cur_tag = None;
                        text_acc.clear();
                    } else {
                        if pt_child_depth == 1 {
                            if let Some(ref tag) = cur_tag {
                                let trimmed = text_acc.trim();
                                if tag == "ele" {
                                    cur_ele = trimmed.parse::<f64>().ok().filter(|v| v.is_finite());
                                } else if tag == "time" {
                                    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed)
                                        .or_else(|_| {
                                            DateTime::parse_from_str(
                                                trimmed,
                                                "%Y-%m-%dT%H:%M:%S%.f%z",
                                            )
                                        })
                                        .or_else(|_| {
                                            DateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S%z")
                                        })
                                    {
                                        cur_time = Some(dt.with_timezone(&Utc));
                                    } else if let Ok(naive) = NaiveDateTime::parse_from_str(
                                        trimmed,
                                        "%Y-%m-%dT%H:%M:%S%.f",
                                    )
                                    .or_else(|_| {
                                        NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S")
                                    }) {
                                        // Per GPX 1.1 spec, unqualified timestamps are UTC.
                                        cur_time = Some(Utc.from_utc_datetime(&naive));
                                    }
                                }
                            }
                            cur_tag = None;
                            text_acc.clear();
                        }
                        pt_child_depth = pt_child_depth.saturating_sub(1);
                    }
                } else if name == "trkseg" || name == "rte" {
                    flush_segment(&mut segments, &mut current_seg);
                }
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(anyhow!("GPX XML parse error: {err}")),
            _ => {}
        }
        buf.clear();
    }

    // Flush any remaining points (including truncated `<trkseg>` on GPS battery loss)
    flush_segment(&mut segments, &mut current_seg);
    segments.sort_by_key(|s| s.points.first().map(|p| p.time_utc));
    Ok(segments)
}

/// Parses and merges one or more `.gpx` files into a unified [`ParsedGpx`] structure.
pub fn load_gpx_files(paths: &[String]) -> Result<ParsedGpx> {
    let mut all_segments = Vec::new();
    for p in paths {
        let mut segs = parse_gpx_file(Path::new(p))?;
        all_segments.append(&mut segs);
    }
    all_segments.sort_by_key(|s| s.points.first().map(|p| p.time_utc));

    let mut point_count = 0usize;
    let mut start_utc: Option<DateTime<Utc>> = None;
    let mut end_utc: Option<DateTime<Utc>> = None;
    for s in &all_segments {
        point_count += s.points.len();
        if let Some(first) = s.points.first() {
            if start_utc.is_none_or(|st| first.time_utc < st) {
                start_utc = Some(first.time_utc);
            }
        }
        if let Some(last) = s.points.last() {
            if end_utc.is_none_or(|en| last.time_utc > en) {
                end_utc = Some(last.time_utc);
            }
        }
    }

    let preview_lines = build_preview_lines(&all_segments, 1800);
    Ok(ParsedGpx {
        files: paths.to_vec(),
        segments: all_segments,
        point_count,
        start_utc,
        end_utc,
        preview_lines,
    })
}

fn build_preview_lines(segments: &[TrackSegment], max_total: usize) -> Vec<Vec<[f64; 2]>> {
    let total: usize = segments.iter().map(|s| s.points.len()).sum();
    if total == 0 {
        return Vec::new();
    }
    let stride = total.div_ceil(max_total.max(1)).max(1);
    segments
        .iter()
        .map(|seg| {
            let mut line = Vec::new();
            for (idx, p) in seg.points.iter().enumerate() {
                if idx == 0 || idx + 1 == seg.points.len() || idx % stride == 0 {
                    line.push([p.lon, p.lat]);
                }
            }
            line
        })
        .filter(|l| !l.is_empty())
        .collect()
}

/// Scans `watched_folder` in parallel for `.gpx` files overlapping `[earliest_utc, latest_utc]`
/// (with ±2 hour padding so slight camera clock drift still surfaces matching tracks).
pub fn find_matching_gpx_files(
    watched_folder: &Path,
    earliest_utc: DateTime<Utc>,
    latest_utc: DateTime<Utc>,
) -> Vec<WatchedGpxMatch> {
    if !watched_folder.is_dir() {
        return Vec::new();
    }
    let (lo, hi) = if earliest_utc <= latest_utc {
        (earliest_utc, latest_utc)
    } else {
        (latest_utc, earliest_utc)
    };
    let pad = Duration::hours(2);
    let win_start = lo - pad;
    let win_end = hi + pad;

    let gpx_paths: Vec<std::path::PathBuf> = WalkDir::new(watched_folder)
        .max_depth(5)
        .follow_links(false)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            let p = e.path();
            let rel = p
                .strip_prefix(watched_folder)
                .unwrap_or(p)
                .to_string_lossy();
            if is_mac_metadata_path(&rel) {
                return None;
            }
            if p.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("gpx"))
            {
                Some(p.to_path_buf())
            } else {
                None
            }
        })
        .collect();

    let mut scored: Vec<(bool, WatchedGpxMatch)> = gpx_paths
        .par_iter()
        .filter_map(|path| {
            let segs = parse_gpx_file(path).ok()?;
            let point_count: usize = segs.iter().map(|s| s.points.len()).sum();
            let start = segs
                .iter()
                .filter_map(|s| s.points.first().map(|p| p.time_utc))
                .min()?;
            let end = segs
                .iter()
                .filter_map(|s| s.points.last().map(|p| p.time_utc))
                .max()?;

            // Check per-segment overlap so multi-trip GPX files do not falsely match gaps between trips
            let mut in_padded_window = false;
            let mut true_overlap = false;
            let mut overlap_seconds = 0i64;
            for seg in &segs {
                let (Some(first), Some(last)) = (seg.points.first(), seg.points.last()) else {
                    continue;
                };
                if last.time_utc >= win_start && first.time_utc <= win_end {
                    in_padded_window = true;
                }
                if last.time_utc >= lo && first.time_utc <= hi {
                    true_overlap = true;
                    let ov_s = first.time_utc.max(lo);
                    let ov_e = last.time_utc.min(hi);
                    overlap_seconds += (ov_e - ov_s).num_seconds().max(0);
                }
            }
            if !in_padded_window {
                return None;
            }
            Some((
                true_overlap,
                WatchedGpxMatch {
                    path: path.to_string_lossy().to_string(),
                    filename: path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    point_count,
                    start_utc: start.to_rfc3339(),
                    end_utc: end.to_rfc3339(),
                    overlap_seconds,
                },
            ))
        })
        .collect();

    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(b.1.overlap_seconds.cmp(&a.1.overlap_seconds))
            .then(a.1.filename.cmp(&b.1.filename))
    });
    scored.into_iter().map(|(_, m)| m).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gpx_ignoring_extensions_and_nan_elevation_and_preserving_routes() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <gpx version="1.1">
          <rte>
            <rtept lat="45.1" lon="-121.1"><time>2026-11-01T20:00:00Z</time></rtept>
          </rte>
          <trk><trkseg>
            <trkpt lat=" 45.3736 " lon="-121.6960">
              <ele>NaN</ele>
              <time>2026-11-01T22:00:00Z</time>
              <extensions><ns:time>2099-01-01T00:00:00Z</ns:time><ns:ele>99999</ns:ele></extensions>
            </trkpt>
            <trkpt lat="45.3836" lon="-121.6860"><ele>1900.5</ele><time>2026-11-01T22:10:00Z</time></trkpt>
          </trkseg></trk>
        </gpx>"#;
        let segs = parse_gpx_str(xml).unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].points.len(), 1);
        assert_eq!(segs[1].points.len(), 2);
        assert_eq!(segs[1].points[0].ele, None); // NaN rejected, extensions ignored!
        assert_eq!(
            segs[1].points[0].time_utc.to_rfc3339(),
            "2026-11-01T22:00:00+00:00"
        );
    }
}
