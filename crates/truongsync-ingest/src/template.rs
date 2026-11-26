use anyhow::Result;
use chrono::{Datelike, NaiveDate};
use std::path::{Path, PathBuf};

const MONTHS_SHORT: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const MONTHS_FULL: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Formats `date` using a single-pass tokenizer (`yyyy`/`YYYY`, `yy`, `MMMM`, `MMM`, `MM`, `M`,
/// `dd`/`DD`, `d`, and quoted literals `'...'`) so tokens never clobber each other or literal text.
/// Default matches Ian's `Smart-Import.ps1`: `"M-dd"` -> `"4-15"`.
pub fn format_date_pattern(date: NaiveDate, pattern: &str) -> String {
    let bytes = pattern.as_bytes();
    let mut i = 0;
    let mut out = String::with_capacity(pattern.len() + 8);
    let m_idx = (date.month().saturating_sub(1) as usize).min(11);

    while i < bytes.len() {
        let rest = &pattern[i..];
        if let Some(after_quote) = rest.strip_prefix('\'') {
            if let Some(end) = after_quote.find('\'') {
                out.push_str(&after_quote[..end]);
                i += end + 2;
                continue;
            }
        }
        if rest.starts_with("yyyy") || rest.starts_with("YYYY") {
            out.push_str(&format!("{:04}", date.year()));
            i += 4;
        } else if rest.starts_with("MMMM") {
            out.push_str(MONTHS_FULL[m_idx]);
            i += 4;
        } else if rest.starts_with("MMM") {
            out.push_str(MONTHS_SHORT[m_idx]);
            i += 3;
        } else if rest.starts_with("yy") || rest.starts_with("YY") {
            out.push_str(&format!("{:02}", date.year().rem_euclid(100)));
            i += 2;
        } else if rest.starts_with("MM") {
            out.push_str(&format!("{:02}", date.month()));
            i += 2;
        } else if rest.starts_with("dd") || rest.starts_with("DD") {
            out.push_str(&format!("{:02}", date.day()));
            i += 2;
        } else if rest.starts_with('M') {
            out.push_str(&date.month().to_string());
            i += 1;
        } else if rest.starts_with('d') || rest.starts_with('D') {
            out.push_str(&date.day().to_string());
            i += 1;
        } else {
            let ch = rest.chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn is_windows_reserved_name(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or("").trim_end();
    matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

/// Sanitizes a single folder segment so macOS and Windows both accept it cleanly
/// (blocks path traversal `..`, illegal characters, trailing dots/spaces, and Windows device names).
pub fn sanitize_segment(input: &str) -> String {
    let trimmed = input.trim();
    let mut s: String = trimmed
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    while s.ends_with('.') || s.ends_with(' ') {
        s.pop();
    }
    if s.is_empty() || s == "." || s == ".." {
        return "Untitled".into();
    }
    if is_windows_reserved_name(&s) {
        s.insert(0, '_');
    }
    if truongsync_db::models::is_mac_metadata_path(&s) {
        s = format!("Shoot_{}", s.trim_start_matches(['.', '_']));
    }
    s
}

#[derive(Debug, Clone)]
pub struct TemplateContext<'a> {
    pub root: &'a Path,
    pub template: &'a str,
    pub date: NaiveDate,
    pub date_format: &'a str,
    pub category: &'a str,
    pub shoot_name: &'a str,
    pub type_folder: &'a str,
}

fn expand_placeholders_single_pass(
    seg: &str,
    year_str: &str,
    date_str: &str,
    category: &str,
    shoot_name: &str,
    type_folder: &str,
) -> String {
    let mut out = String::with_capacity(seg.len() + 16);
    let mut rest = seg;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        if let Some(close) = after.find('}') {
            let token = &after[..close];
            match token.to_ascii_lowercase().as_str() {
                "year" => out.push_str(year_str),
                "category" => out.push_str(category.trim()),
                "shootname" => out.push_str(shoot_name.trim()),
                "date" => out.push_str(date_str),
                "type" => out.push_str(type_folder.trim()),
                _ => {
                    out.push('{');
                    out.push_str(token);
                    out.push('}');
                }
            }
            rest = &after[close + 1..];
        } else {
            out.push_str(&rest[open..]);
            return out;
        }
    }
    out.push_str(rest);
    out
}

/// Resolves a destination directory from a universal folder template in a single pass
/// (immune to token re-expansion if a shoot name contains `{Date}` etc.).
pub fn resolve_destination_dir(ctx: &TemplateContext<'_>) -> PathBuf {
    let year_str = format!("{:04}", ctx.date.year());
    let date_str = format_date_pattern(ctx.date, ctx.date_format);
    let mut dest = ctx.root.to_path_buf();

    for raw_seg in ctx.template.replace('\\', "/").split('/') {
        let seg = raw_seg.trim();
        if seg.is_empty() {
            continue;
        }
        let replaced = expand_placeholders_single_pass(
            seg,
            &year_str,
            &date_str,
            ctx.category,
            ctx.shoot_name,
            ctx.type_folder,
        );
        let cleaned = replaced.trim();
        if cleaned.is_empty() {
            continue;
        }
        dest.push(sanitize_segment(cleaned));
    }
    dest
}

/// Resolves the shoot root folder (the ancestor directory preceding any `{Type}` segment).
pub fn resolve_shoot_root_dir(ctx: &TemplateContext<'_>) -> PathBuf {
    let before_type = ctx
        .template
        .replace('\\', "/")
        .split('/')
        .take_while(|s| !s.to_ascii_lowercase().contains("{type}"))
        .collect::<Vec<_>>()
        .join("/");
    let sub = TemplateContext {
        template: &before_type,
        type_folder: "",
        ..ctx.clone()
    };
    resolve_destination_dir(&sub)
}

/// Lists existing category subdirectories inside `{base_root}/{year}`, optionally initializing
/// `default_categories` if the year folder does not exist yet (matching `Smart-Import.ps1`).
pub fn list_or_init_year_categories(
    base_root: &Path,
    year: i32,
    default_categories: &[String],
    create_if_missing: bool,
) -> Result<Vec<String>> {
    let year_dir = base_root.join(format!("{year:04}"));
    if !year_dir.exists() {
        if create_if_missing && !base_root.as_os_str().is_empty() {
            std::fs::create_dir_all(&year_dir)?;
            for cat in default_categories {
                let c = sanitize_segment(cat);
                if !c.is_empty() {
                    std::fs::create_dir_all(year_dir.join(c))?;
                }
            }
        } else {
            let mut defaults = default_categories.to_vec();
            defaults.sort();
            return Ok(defaults);
        }
    }

    let mut categories = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&year_dir) {
        for entry in rd.flatten() {
            if entry.path().is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.starts_with('.') {
                    categories.push(name);
                }
            }
        }
    }
    if categories.is_empty() {
        categories = default_categories.to_vec();
    }
    categories.sort_by_key(|s| s.to_lowercase());
    Ok(categories)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_default_smart_import_template_and_edge_cases() {
        let d = NaiveDate::from_ymd_opt(2026, 4, 15).unwrap();
        assert_eq!(format_date_pattern(d, "M-dd"), "4-15");
        assert_eq!(format_date_pattern(d, "yyyy-MM-dd"), "2026-04-15");
        assert_eq!(format_date_pattern(d, "MMM dd, yyyy"), "Apr 15, 2026");
        assert_eq!(format_date_pattern(d, "yy-MM-dd"), "26-04-15");

        assert_eq!(sanitize_segment("CON"), "_CON");
        assert_eq!(sanitize_segment("aux.shoot"), "_aux.shoot");
        assert_eq!(sanitize_segment(".."), "Untitled");

        let root = PathBuf::from("/Photos/Photo Dump");
        let dest = resolve_destination_dir(&TemplateContext {
            root: &root,
            template: "{Year}/{Category}/{ShootName} {Date}/{Type}",
            date: d,
            date_format: "M-dd",
            category: "Hikes",
            shoot_name: "Cone Peak {Type}",
            type_folder: "VID",
        });
        // Single-pass prevents `{Type}` inside shoot_name from expanding into `VID`
        assert_eq!(
            dest,
            PathBuf::from("/Photos/Photo Dump/2026/Hikes/Cone Peak {Type} 4-15/VID")
        );
    }
}
