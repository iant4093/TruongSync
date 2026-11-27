use anyhow::Result;
use chrono::{DateTime, Duration, FixedOffset, NaiveDateTime, Utc};
use nom_exif::{EntryValue, Exif, ExifTag, MediaParser, MediaSource};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use truongsync_db::models::{is_mac_metadata_path, AppSettings};
use truongsync_ingest::transfer::{classify_extension, resolve_local_naive_to_utc, MediaKind};
use walkdir::WalkDir;

use crate::gpx::{ParsedGpx, TrackPoint};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchStatus {
    Matched,
    AlreadyTagged,
    OutOfRange,
    NoTimestamp,
    SkippedVideo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhotoGpsMatch {
    pub path: String,
    pub filename: String,
    pub media_kind: String,
    pub capture_time_utc: Option<String>,
    pub has_existing_gps: bool,
    pub status: MatchStatus,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub ele: Option<f64>,
    pub nearest_point_delta_secs: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeotagPlan {
    pub gpx_files: Vec<String>,
    pub track_point_count: usize,
    pub track_start_utc: Option<String>,
    pub track_end_utc: Option<String>,
    pub preview_lines: Vec<Vec<[f64; 2]>>,
    pub items: Vec<PhotoGpsMatch>,
    pub matched_count: usize,
    pub already_tagged_count: usize,
    pub out_of_range_count: usize,
    pub skipped_video_count: usize,
}

/// Strictly interpolates `(lat, lon, ele, nearest_delta_secs)` for `target_utc` inside `gpx`.
/// Evaluates ALL covering segments (picking the smallest nearest-point delta), handles exact
/// endpoint matches before gap checks, handles antimeridian (±180°) wrap-around, and strictly
/// returns `None` outside every segment or when bracketing points exceed `max_gap_secs`.
pub fn interpolate_strict_in_range(
    gpx: &ParsedGpx,
    target_utc: DateTime<Utc>,
    max_gap_secs: i64,
) -> Option<(f64, f64, Option<f64>, i64)> {
    let max_gap_ms = max_gap_secs.max(0).saturating_mul(1000);
    let mut best: Option<(f64, f64, Option<f64>, i64)> = None;

    for seg in &gpx.segments {
        let pts = &seg.points;
        if pts.is_empty() {
            continue;
        }
        let first = pts[0];
        let last = pts[pts.len() - 1];
        if target_utc < first.time_utc || target_utc > last.time_utc {
            continue;
        }

        let candidate = if pts.len() == 1 {
            Some((first.lat, first.lon, first.ele, 0i64))
        } else {
            let idx = pts.partition_point(|p| p.time_utc < target_utc);
            if idx == 0 {
                Some((pts[0].lat, pts[0].lon, pts[0].ele, 0))
            } else if idx >= pts.len() {
                let p = pts[pts.len() - 1];
                Some((p.lat, p.lon, p.ele, 0))
            } else {
                let p0: TrackPoint = pts[idx - 1];
                let p1: TrackPoint = pts[idx];
                // Exact match on p1 requires zero interpolation even if preceded by a large gap
                if p1.time_utc == target_utc {
                    Some((p1.lat, p1.lon, p1.ele, 0))
                } else {
                    let span_ms = (p1.time_utc - p0.time_utc).num_milliseconds();
                    if span_ms > max_gap_ms {
                        None
                    } else {
                        let dt = (target_utc - p0.time_utc).num_milliseconds() as f64;
                        let total = span_ms.max(1) as f64;
                        let frac = (dt / total).clamp(0.0, 1.0);
                        let lat = p0.lat + (p1.lat - p0.lat) * frac;

                        // Antimeridian-safe shortest-arc longitude interpolation
                        let mut dlon = p1.lon - p0.lon;
                        if dlon > 180.0 {
                            dlon -= 360.0;
                        } else if dlon < -180.0 {
                            dlon += 360.0;
                        }
                        let mut lon = p0.lon + dlon * frac;
                        if lon > 180.0 {
                            lon -= 360.0;
                        } else if lon < -180.0 {
                            lon += 360.0;
                        }

                        let ele = match (p0.ele, p1.ele) {
                            (Some(e0), Some(e1)) => Some(e0 + (e1 - e0) * frac),
                            (Some(e), None) | (None, Some(e)) => Some(e),
                            (None, None) => None,
                        };
                        let d0 = (target_utc - p0.time_utc).num_seconds().abs();
                        let d1 = (p1.time_utc - target_utc).num_seconds().abs();
                        Some((lat, lon, ele, d0.min(d1)))
                    }
                }
            }
        };

        if let Some(cand) = candidate {
            if best.is_none_or(|b| cand.3 < b.3) {
                best = Some(cand);
            }
        }
    }

    best
}

/// Lightweight Canon `.CR3` GPS detector: `nom-exif 3.8` parses `CMT1..CMT3` and skips `CMT4`
/// (Canon's GPS IFD box inside `moov -> uuid(85c0b687-820f-11e0-8111-f4ce462b6a48)`).
/// Inspecting `CMT4` directly guarantees `overwrite_existing = false` never clobbers already-tagged `.CR3`s!
pub fn cr3_has_gps_box(path: &Path) -> bool {
    const CANON_UUID: [u8; 16] = [
        0x85, 0xC0, 0xB6, 0x87, 0x82, 0x0F, 0x11, 0xE0, 0x81, 0x11, 0xF4, 0xCE, 0x46, 0x2B, 0x6A,
        0x48,
    ];
    let Ok(mut f) = File::open(path) else {
        return false;
    };
    let Ok(file_len) = f.metadata().map(|m| m.len()) else {
        return false;
    };
    let mut pos = 0u64;
    let mut hdr = [0u8; 16];
    // Walk top-level ISO-BMFF boxes to find `moov` (strictly checked arithmetic against corrupt/malicious boxes)
    while file_len.saturating_sub(pos) >= 8 {
        if f.seek(SeekFrom::Start(pos)).is_err() || f.read_exact(&mut hdr[..8]).is_err() {
            break;
        }
        let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as u64;
        let box_type = [hdr[4], hdr[5], hdr[6], hdr[7]];
        let (box_size, header_len) = if size32 == 1 {
            if file_len.saturating_sub(pos) < 16 || f.read_exact(&mut hdr[8..16]).is_err() {
                break;
            }
            (u64::from_be_bytes(hdr[8..16].try_into().unwrap()), 16u64)
        } else if size32 == 0 {
            (file_len - pos, 8u64)
        } else {
            (size32, 8u64)
        };
        if box_size < header_len || pos.checked_add(box_size).is_none_or(|end| end > file_len) {
            break;
        }
        if &box_type == b"moov" {
            let moov_len = (box_size - header_len).min(8 * 1024 * 1024) as usize;
            let mut buf = vec![0u8; moov_len];
            if f.read_exact(&mut buf).is_ok() {
                return scan_moov_for_cmt4_gps(&buf, &CANON_UUID);
            }
            return false;
        }
        pos += box_size;
    }
    false
}

fn next_bmff_box(data: &[u8], pos: usize) -> Option<([u8; 4], usize, usize)> {
    let rem = data.len().checked_sub(pos)?;
    if rem < 8 {
        return None;
    }
    let size32 =
        u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as u64;
    let box_type = [data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]];
    let (box_size, header_len) = if size32 == 1 {
        if rem < 16 {
            return None;
        }
        let sz = u64::from_be_bytes(data[pos + 8..pos + 16].try_into().ok()?);
        (sz, 16u64)
    } else if size32 == 0 {
        (rem as u64, 8u64)
    } else {
        (size32, 8u64)
    };
    if box_size < header_len || box_size > rem as u64 {
        return None;
    }
    let payload_start = pos.checked_add(header_len as usize)?;
    let end = pos.checked_add(box_size as usize)?;
    Some((box_type, payload_start, end))
}

fn scan_moov_for_cmt4_gps(moov: &[u8], canon_uuid: &[u8; 16]) -> bool {
    let mut p = 0usize;
    while let Some((box_type, payload_start, end)) = next_bmff_box(moov, p) {
        if &box_type == b"uuid"
            && end.saturating_sub(payload_start) >= 16
            && &moov[payload_start..payload_start + 16] == canon_uuid
        {
            let inner = &moov[payload_start + 16..end];
            let mut q = 0usize;
            while let Some((sub_type, sub_start, sub_end)) = next_bmff_box(inner, q) {
                if &sub_type == b"CMT4" {
                    return tiff_ifd_has_lat_lon(&inner[sub_start..sub_end]);
                }
                q = sub_end;
            }
        }
        p = end;
    }
    false
}

fn tiff_ifd_has_lat_lon(tiff: &[u8]) -> bool {
    if tiff.len() < 10 {
        return false;
    }
    let le = match &tiff[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return false,
    };
    let r16 = |o: usize| -> Option<u16> {
        let s = tiff.get(o..o.checked_add(2)?)?;
        Some(if le {
            u16::from_le_bytes([s[0], s[1]])
        } else {
            u16::from_be_bytes([s[0], s[1]])
        })
    };
    let r32 = |o: usize| -> Option<u32> {
        let s = tiff.get(o..o.checked_add(4)?)?;
        Some(if le {
            u32::from_le_bytes([s[0], s[1], s[2], s[3]])
        } else {
            u32::from_be_bytes([s[0], s[1], s[2], s[3]])
        })
    };
    // Verify TIFF magic number 42
    if r16(2) != Some(42) {
        return false;
    }
    let Some(ifd_off) = r32(4).map(|v| v as usize) else {
        return false;
    };
    let Some(count) = r16(ifd_off).map(|v| v as usize) else {
        return false;
    };
    let mut has_lat = false;
    let mut has_lon = false;
    for i in 0..count.min(64) {
        let Some(entry_off) = ifd_off.checked_add(2).and_then(|v| v.checked_add(i * 12)) else {
            break;
        };
        if entry_off.checked_add(12).is_none_or(|e| e > tiff.len()) {
            break;
        }
        if let (Some(tag), Some(val_count)) = (r16(entry_off), r32(entry_off + 4)) {
            if val_count > 0 {
                if tag == 0x0002 {
                    has_lat = true;
                } else if tag == 0x0004 {
                    has_lon = true;
                }
            }
        }
    }
    has_lat && has_lon
}

fn parse_fixed_offset(raw: &str) -> Option<FixedOffset> {
    let trimmed = raw.trim_matches(|c: char| c.is_whitespace() || c == '\0');
    if trimmed.eq_ignore_ascii_case("Z") {
        return FixedOffset::east_opt(0);
    }
    trimmed.parse::<FixedOffset>().ok()
}

fn read_photo_exif_time_and_gps(
    path: &Path,
    parser: &mut MediaParser,
) -> (Option<DateTime<Utc>>, bool) {
    let is_cr3 = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("cr3"));

    let Ok(ms) = MediaSource::open(path) else {
        return (None, is_cr3 && cr3_has_gps_box(path));
    };
    let Ok(iter) = parser.parse_exif(ms) else {
        return (None, is_cr3 && cr3_has_gps_box(path));
    };
    let exif: Exif = iter.into();

    let mut has_gps =
        exif.get(ExifTag::GPSLatitude).is_some() && exif.get(ExifTag::GPSLongitude).is_some();
    if !has_gps && is_cr3 {
        has_gps = cr3_has_gps_box(path);
    }

    let offset_for = |tag: ExifTag| -> Option<FixedOffset> {
        let candidates: &[ExifTag] = match tag {
            ExifTag::DateTimeOriginal => {
                &[ExifTag::OffsetTimeOriginal, ExifTag::OffsetTimeDigitized]
            }
            ExifTag::CreateDate => &[ExifTag::OffsetTimeDigitized, ExifTag::OffsetTimeOriginal],
            _ => &[ExifTag::OffsetTime],
        };
        candidates.iter().find_map(|&ot| match exif.get(ot) {
            Some(EntryValue::Text(s)) => parse_fixed_offset(s),
            _ => None,
        })
    };

    let mut dt_utc: Option<DateTime<Utc>> = None;
    for tag in [ExifTag::DateTimeOriginal, ExifTag::CreateDate] {
        if let Some(val) = exif.get(tag) {
            let explicit_offset = offset_for(tag);
            match val {
                EntryValue::DateTime(dt) => {
                    dt_utc = Some(dt.with_timezone(&Utc));
                    break;
                }
                EntryValue::NaiveDateTime(naive) => {
                    dt_utc = explicit_offset
                        .and_then(|off| naive.and_local_timezone(off).single())
                        .map(|d| d.with_timezone(&Utc))
                        .or_else(|| Some(resolve_local_naive_to_utc(*naive)));
                    break;
                }
                _ => {
                    let clean = val.to_string();
                    let s = clean
                        .trim_matches(|c: char| c.is_whitespace() || c == '\0')
                        .trim_matches('"');
                    if let Ok(parsed) = DateTime::parse_from_rfc3339(s) {
                        dt_utc = Some(parsed.with_timezone(&Utc));
                        break;
                    }
                    if let Ok(naive) = NaiveDateTime::parse_from_str(s, "%Y:%m:%d %H:%M:%S")
                        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S"))
                    {
                        dt_utc = explicit_offset
                            .and_then(|off| naive.and_local_timezone(off).single())
                            .map(|d| d.with_timezone(&Utc))
                            .or_else(|| Some(resolve_local_naive_to_utc(naive)));
                        break;
                    }
                }
            }
        }
    }

    (dt_utc, has_gps)
}

/// Scans `target_paths` (folders or individual files) and matches photos (RAW + JPEG only;
/// videos are strictly skipped!) against `gpx`.
pub fn build_geotag_plan(
    target_paths: &[String],
    gpx: &ParsedGpx,
    clock_offset_secs: i64,
    max_gap_secs: i64,
    overwrite_existing: bool,
    settings: &AppSettings,
) -> Result<GeotagPlan> {
    let mut seen_canon: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut files: Vec<(PathBuf, MediaKind)> = Vec::new();
    let mut push_unique = |path: PathBuf, kind: MediaKind| {
        let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if seen_canon.insert(key) {
            files.push((path, kind));
        }
    };
    for raw in target_paths {
        let p = Path::new(raw);
        if p.is_file() {
            if !is_mac_metadata_path(raw) {
                if let Some(k) = classify_extension(p, settings) {
                    push_unique(p.to_path_buf(), k);
                }
            }
        } else if p.is_dir() {
            for entry in WalkDir::new(p).follow_links(false).into_iter().flatten() {
                if !entry.file_type().is_file() {
                    continue;
                }
                let fp = entry.path();
                let rel = fp.strip_prefix(p).unwrap_or(fp).to_string_lossy();
                if is_mac_metadata_path(&rel) {
                    continue;
                }
                if let Some(k) = classify_extension(fp, settings) {
                    push_unique(fp.to_path_buf(), k);
                }
            }
        }
    }

    if !(-31_536_000..=31_536_000).contains(&clock_offset_secs) {
        return Err(anyhow::anyhow!(
            "Camera clock offset must be within ±365 days (±31,536,000 seconds)."
        ));
    }
    let offset = Duration::try_seconds(clock_offset_secs)
        .ok_or_else(|| anyhow::anyhow!("clock_offset_secs out of valid range"))?;
    let mut items: Vec<PhotoGpsMatch> = files
        .par_iter()
        .map_init(MediaParser::new, |parser, (path, kind)| {
            let filename = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let path_str = path.to_string_lossy().to_string();
            let media_kind = match kind {
                MediaKind::Raw => "raw",
                MediaKind::Jpeg => "jpeg",
                MediaKind::Video => "video",
            }
            .to_string();

            // STRICT RULE: Videos are NEVER geotagged.
            if matches!(kind, MediaKind::Video) {
                return PhotoGpsMatch {
                    path: path_str,
                    filename,
                    media_kind,
                    capture_time_utc: None,
                    has_existing_gps: false,
                    status: MatchStatus::SkippedVideo,
                    lat: None,
                    lon: None,
                    ele: None,
                    nearest_point_delta_secs: None,
                };
            }

            let (raw_ts, has_existing) = read_photo_exif_time_and_gps(path, parser);
            let Some(ts) = raw_ts else {
                return PhotoGpsMatch {
                    path: path_str,
                    filename,
                    media_kind,
                    capture_time_utc: None,
                    has_existing_gps: has_existing,
                    status: MatchStatus::NoTimestamp,
                    lat: None,
                    lon: None,
                    ele: None,
                    nearest_point_delta_secs: None,
                };
            };

            let Some(adjusted) = ts.checked_add_signed(offset) else {
                return PhotoGpsMatch {
                    path: path_str,
                    filename,
                    media_kind,
                    capture_time_utc: None,
                    has_existing_gps: has_existing,
                    status: MatchStatus::OutOfRange,
                    lat: None,
                    lon: None,
                    ele: None,
                    nearest_point_delta_secs: None,
                };
            };
            if has_existing && !overwrite_existing {
                return PhotoGpsMatch {
                    path: path_str,
                    filename,
                    media_kind,
                    capture_time_utc: Some(adjusted.to_rfc3339()),
                    has_existing_gps: true,
                    status: MatchStatus::AlreadyTagged,
                    lat: None,
                    lon: None,
                    ele: None,
                    nearest_point_delta_secs: None,
                };
            }

            match interpolate_strict_in_range(gpx, adjusted, max_gap_secs) {
                Some((lat, lon, ele, delta)) => PhotoGpsMatch {
                    path: path_str,
                    filename,
                    media_kind,
                    capture_time_utc: Some(adjusted.to_rfc3339()),
                    has_existing_gps: has_existing,
                    status: MatchStatus::Matched,
                    lat: Some(lat),
                    lon: Some(lon),
                    ele,
                    nearest_point_delta_secs: Some(delta),
                },
                None => PhotoGpsMatch {
                    path: path_str,
                    filename,
                    media_kind,
                    capture_time_utc: Some(adjusted.to_rfc3339()),
                    has_existing_gps: has_existing,
                    status: MatchStatus::OutOfRange,
                    lat: None,
                    lon: None,
                    ele: None,
                    nearest_point_delta_secs: None,
                },
            }
        })
        .collect();

    items.sort_by(|a, b| a.filename.cmp(&b.filename).then(a.path.cmp(&b.path)));

    let matched_count = items
        .iter()
        .filter(|i| i.status == MatchStatus::Matched)
        .count();
    let already_tagged_count = items
        .iter()
        .filter(|i| i.status == MatchStatus::AlreadyTagged)
        .count();
    let out_of_range_count = items
        .iter()
        .filter(|i| i.status == MatchStatus::OutOfRange)
        .count();
    let skipped_video_count = items
        .iter()
        .filter(|i| i.status == MatchStatus::SkippedVideo)
        .count();

    Ok(GeotagPlan {
        gpx_files: gpx.files.clone(),
        track_point_count: gpx.point_count,
        track_start_utc: gpx.start_utc.map(|t| t.to_rfc3339()),
        track_end_utc: gpx.end_utc.map(|t| t.to_rfc3339()),
        preview_lines: gpx.preview_lines.clone(),
        items,
        matched_count,
        already_tagged_count,
        out_of_range_count,
        skipped_video_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpx::{parse_gpx_str, ParsedGpx};

    #[test]
    fn interpolation_rejects_out_of_range_handles_overlapping_tracks_exact_p1_and_antimeridian() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <gpx version="1.1">
          <trk>
            <trkseg>
              <trkpt lat="45.0" lon="179.9"><ele>1000</ele><time>2026-11-01T10:00:00Z</time></trkpt>
              <trkpt lat="46.0" lon="-179.9"><ele>2000</ele><time>2026-11-01T10:30:00Z</time></trkpt>
            </trkseg>
            <trkseg>
              <trkpt lat="45.5" lon="179.95"><ele>1500</ele><time>2026-11-01T10:10:00Z</time></trkpt>
              <trkpt lat="45.6" lon="-179.95"><ele>1600</ele><time>2026-11-01T10:20:00Z</time></trkpt>
            </trkseg>
          </trk>
        </gpx>"#;
        let segs = parse_gpx_str(xml).unwrap();
        let gpx = ParsedGpx {
            files: vec![],
            segments: segs,
            point_count: 4,
            start_utc: None,
            end_utc: None,
            preview_lines: vec![],
        };

        // 10:15 falls in a 30-min gap in seg 1 (>15m max_gap), MUST continue to seg 2 & interpolate across ±180°!
        let mid = DateTime::parse_from_rfc3339("2026-11-01T10:15:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let (lat, lon, _ele, _) = interpolate_strict_in_range(&gpx, mid, 900).unwrap();
        assert!((lat - 45.55).abs() < 1e-6);
        assert!(lon.abs() > 179.99, "expected antimeridian ~±180, got {lon}");

        // Exact p1 match at 10:30 even though seg 1 has a 30-min gap before it
        let exact_p1 = DateTime::parse_from_rfc3339("2026-11-01T10:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let (lat2, lon2, _, delta2) = interpolate_strict_in_range(&gpx, exact_p1, 60).unwrap();
        assert!((lat2 - 46.0).abs() < 1e-6);
        assert!((lon2 - -179.9).abs() < 1e-6);
        assert_eq!(delta2, 0);

        // Outside track window MUST return None
        let after = DateTime::parse_from_rfc3339("2026-11-01T10:30:01Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(interpolate_strict_in_range(&gpx, after, 900).is_none());
    }
}
