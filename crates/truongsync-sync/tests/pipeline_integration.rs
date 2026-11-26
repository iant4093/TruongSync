use tempfile::tempdir;
use tokio_util::sync::CancellationToken;
use truongsync_db::models::{AppSettings, ExternalProfile, ManifestEntry};
use truongsync_db::DbHandle;
use truongsync_geo::exiftool::{check_exiftool, write_geotags_batch};
use truongsync_geo::gpx::{find_matching_gpx_files, load_gpx_files};
use truongsync_geo::matcher::{build_geotag_plan, MatchStatus};
use truongsync_ingest::template::{
    list_or_init_year_categories, resolve_destination_dir, TemplateContext,
};
use truongsync_ingest::transfer::{
    build_import_plan, execute_import_plan, run_preflight_checks, scan_media_source,
    ShootAssignment,
};

/// Minimal valid 1x1 JPEG with no GPS tags for ExifTool roundtrip testing.
const TINY_JPEG: &[u8] = &[
    0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x00, 0x00, 0x01,
    0x00, 0x01, 0x00, 0x00, 0xFF, 0xDB, 0x00, 0x43, 0x00, 0x08, 0x06, 0x06, 0x07, 0x06, 0x05, 0x08,
    0x07, 0x07, 0x07, 0x09, 0x09, 0x08, 0x0A, 0x0C, 0x14, 0x0D, 0x0C, 0x0B, 0x0B, 0x0C, 0x19, 0x12,
    0x13, 0x0F, 0x14, 0x1D, 0x1A, 0x1F, 0x1E, 0x1D, 0x1A, 0x1C, 0x1C, 0x20, 0x24, 0x2E, 0x27, 0x20,
    0x22, 0x2C, 0x23, 0x1C, 0x1C, 0x28, 0x37, 0x29, 0x2C, 0x30, 0x31, 0x34, 0x34, 0x34, 0x1F, 0x27,
    0x39, 0x3D, 0x38, 0x32, 0x3C, 0x2E, 0x33, 0x34, 0x32, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x01,
    0x00, 0x01, 0x01, 0x01, 0x11, 0x00, 0xFF, 0xC4, 0x00, 0x14, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x08, 0xFF, 0xC4, 0x00, 0x14,
    0x10, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00, 0x7F, 0x00, 0xFF, 0xD9,
];

#[test]
fn end_to_end_multiday_import_geotag_and_tombstone_db() {
    let dir = tempdir().unwrap();
    let db = DbHandle::open(&dir.path().join("state.db")).unwrap();
    let settings = AppSettings::default();

    // 1. Multi-day SD card layout with Mac junk + real RAW/JPG/MP4
    let sd = dir.path().join("SD_CARD/DCIM/100EOSR7");
    std::fs::create_dir_all(&sd).unwrap();
    let dest_root = dir.path().join("Photo Dump");
    std::fs::create_dir_all(&dest_root).unwrap();

    let cats =
        list_or_init_year_categories(&dest_root, 2026, &settings.default_categories, true).unwrap();
    assert!(cats.contains(&"Hikes".to_string()));
    assert!(cats.contains(&"Wildlife".to_string()));

    let jpg_path = sd.join("IMG_1001.JPG");
    let mp4_path = sd.join("CLIP_1001.MP4");
    std::fs::write(&jpg_path, TINY_JPEG).unwrap();
    std::fs::write(&mp4_path, b"fake-mp4-video-payload").unwrap();
    std::fs::write(sd.join("._IMG_1001.JPG"), b"mac-appledouble").unwrap();

    // If exiftool is available on this machine, stamp a known DateTimeOriginal onto IMG_1001.JPG
    let et = check_exiftool(None);
    if et.available {
        let status = std::process::Command::new(et.path.as_ref().unwrap())
            .args([
                "-overwrite_original",
                "-DateTimeOriginal=2026:11:01 10:05:00+00:00",
                "-OffsetTimeOriginal=+00:00",
            ])
            .arg(&jpg_path)
            .status()
            .unwrap();
        assert!(status.success());
    }

    let scan = scan_media_source(&dir.path().join("SD_CARD"), &settings).unwrap();
    assert_eq!(scan.jpeg_count, 1);
    assert_eq!(scan.video_count, 1);
    assert_eq!(scan.ignored_mac_metadata_count, 1);

    let preflight = run_preflight_checks(&scan, &dest_root, true);
    assert!(preflight.blockers.is_empty());

    let plan = build_import_plan(
        &scan,
        &dest_root,
        &settings.folder_template,
        &settings.date_format,
        &settings.raw_folder_name,
        &settings.jpeg_folder_name,
        &settings.video_folder_name,
        &[ShootAssignment {
            dates: vec![],
            shoot_name: "Mt Hood".into(),
            category: "Hikes".into(),
            folder_date: "2026-11-01".into(),
        }],
    )
    .unwrap();

    let cancel = CancellationToken::new();
    let moved = execute_import_plan(&plan, true, 4, true, &cancel, |_| {}).unwrap();
    assert_eq!(moved.files_moved, 2);
    assert_eq!(moved.files_failed, 0);
    assert!(!jpg_path.exists());
    assert!(!mp4_path.exists());

    let shoot_dir = resolve_destination_dir(&TemplateContext {
        root: &dest_root,
        template: "{Year}/{Category}/{ShootName} {Date}",
        date: chrono::NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(),
        date_format: "M-dd",
        category: "Hikes",
        shoot_name: "Mt Hood",
        type_folder: "",
    });
    assert!(shoot_dir.join("JPEG/IMG_1001.JPG").exists());
    assert!(shoot_dir.join("VID/CLIP_1001.MP4").exists());

    // 2. GPX auto-match + geotagging (strictly skips video!)
    let gpx_dir = dir.path().join("GPX");
    std::fs::create_dir_all(&gpx_dir).unwrap();
    let gpx_file = gpx_dir.join("mthood.gpx");
    std::fs::write(
        &gpx_file,
        r#"<?xml version="1.0" encoding="UTF-8"?>
        <gpx version="1.1"><trk><trkseg>
            <trkpt lat="45.3700" lon="-121.6900"><ele>1800</ele><time>2026-11-01T10:00:00Z</time></trkpt>
            <trkpt lat="45.3800" lon="-121.7000"><ele>2000</ele><time>2026-11-01T10:10:00Z</time></trkpt>
        </trkseg></trk></gpx>"#,
    )
    .unwrap();

    let auto_matches = find_matching_gpx_files(
        &gpx_dir,
        chrono::DateTime::parse_from_rfc3339("2026-11-01T10:02:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
        chrono::DateTime::parse_from_rfc3339("2026-11-01T10:08:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
    );
    assert_eq!(auto_matches.len(), 1);

    if et.available {
        let gpx = load_gpx_files(&[gpx_file.to_string_lossy().to_string()]).unwrap();
        let gt_plan = build_geotag_plan(
            &[shoot_dir.to_string_lossy().to_string()],
            &gpx,
            0,
            900,
            false,
            &settings,
        )
        .unwrap();
        assert_eq!(gt_plan.matched_count, 1);
        assert_eq!(gt_plan.skipped_video_count, 1);
        assert!(gt_plan
            .items
            .iter()
            .any(|i| i.filename == "CLIP_1001.MP4" && i.status == MatchStatus::SkippedVideo));

        let gt_res =
            write_geotags_batch(&gt_plan.items, None, true, &cancel, |_, _, _| {}).unwrap();
        assert_eq!(gt_res.tagged_count, 1);
        assert_eq!(gt_res.failed_count, 0);
        assert_ne!(
            gt_res.updated_records[0].pre_md5,
            gt_res.updated_records[0].post_md5
        );

        // Record intentional backfill update for safe Google Drive replacement
        db.record_intentional_update(
            &gt_res.updated_records[0].path,
            &gt_res.updated_records[0].pre_md5,
            &gt_res.updated_records[0].post_md5,
            "gps_backfill",
        )
        .unwrap();
        assert_eq!(db.list_intentional_updates().unwrap().len(), 1);
    }

    // 3. External profile & tombstone tracking roundtrip
    let pid = db
        .upsert_external_profile(&ExternalProfile {
            id: "".into(),
            name: "Extreme SSD".into(),
            local_root: dest_root.to_string_lossy().to_string(),
            volume_uuid: "TEST-UUID-123".into(),
            volume_label: "Extreme SSD".into(),
            relative_dest_path: "Photo Library Backup/All Photos".into(),
            direction: "two_way_additive".into(),
            enabled: true,
            schedule_mode: "on_connect".into(),
            schedule_value: "".into(),
            schedule_action: "check".into(),
            write_marker_file: true,
            last_sync_at: None,
            last_verify_at: None,
            last_status: "never".into(),
            last_error: None,
            pending_push_count: 0,
            pending_pull_count: 0,
            pending_bytes: 0,
            stale_after_days: 14,
        })
        .unwrap();
    db.upsert_manifest_entries(&[ManifestEntry {
        profile_kind: "external".into(),
        profile_id: pid.clone(),
        rel_path: "2026/Hikes/Mt Hood 11-01/JPEG/IMG_1001.JPG".into(),
        size_bytes: 200,
        md5_hash: None,
        mtime_unix: 1700000000,
        state: "synced".into(),
    }])
    .unwrap();
    db.mark_local_tombstones(
        "external",
        &pid,
        &["2026/Hikes/Mt Hood 11-01/JPEG/IMG_1001.JPG".into()],
    )
    .unwrap();
    assert_eq!(
        db.load_manifest_map("external", &pid)
            .unwrap()
            .get("2026/Hikes/Mt Hood 11-01/JPEG/IMG_1001.JPG")
            .unwrap()
            .state,
        "local_tombstone"
    );
}

#[test]
fn geotag_refuses_video_and_writes_below_sea_level_altitude_ref() {
    let et = check_exiftool(None);
    if !et.available {
        return;
    }
    let dir = tempdir().unwrap();
    let mp4_path = dir.path().join("FAKE_VIDEO.MP4");
    let jpg_path = dir.path().join("DEATH_VALLEY.JPG");
    std::fs::write(&mp4_path, b"do-not-touch-video-bytes").unwrap();
    std::fs::write(&jpg_path, TINY_JPEG).unwrap();

    // Intentionally lie about media_kind ("raw") on an MP4 to verify defense-in-depth video refusal!
    let items = vec![
        truongsync_geo::matcher::PhotoGpsMatch {
            path: mp4_path.to_string_lossy().to_string(),
            filename: "FAKE_VIDEO.MP4".into(),
            media_kind: "raw".into(),
            capture_time_utc: Some("2026-11-01T10:05:00Z".into()),
            has_existing_gps: false,
            status: MatchStatus::Matched,
            lat: Some(36.2297),
            lon: Some(-116.7669),
            ele: Some(-86.0),
            nearest_point_delta_secs: Some(0),
        },
        truongsync_geo::matcher::PhotoGpsMatch {
            path: jpg_path.to_string_lossy().to_string(),
            filename: "DEATH_VALLEY.JPG".into(),
            media_kind: "jpeg".into(),
            capture_time_utc: Some("2026-11-01T10:05:00Z".into()),
            has_existing_gps: false,
            status: MatchStatus::Matched,
            lat: Some(36.2297),
            lon: Some(-116.7669),
            ele: Some(-86.0),
            nearest_point_delta_secs: Some(0),
        },
    ];

    let cancel = CancellationToken::new();
    let res = write_geotags_batch(&items, None, true, &cancel, |_, _, _| {}).unwrap();
    assert_eq!(res.tagged_count, 1);
    assert_eq!(res.skipped_count, 1);
    assert_eq!(res.failed_count, 0);
    assert_eq!(
        std::fs::read(&mp4_path).unwrap(),
        b"do-not-touch-video-bytes"
    );

    // Read back raw numeric GPSAltitudeRef to prove Below Sea Level (= 1) was written
    let out = std::process::Command::new(et.path.as_ref().unwrap())
        .args(["-n", "-s3", "-GPSAltitudeRef", "-GPSAltitude"])
        .arg(&jpg_path)
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(
        lines[0], "1",
        "Expected GPSAltitudeRef=1 (Below Sea Level), got {:?}",
        lines
    );
    let alt: f64 = lines[1].parse().unwrap();
    assert!(
        (alt.abs() - 86.0).abs() < 0.1,
        "Expected altitude magnitude ~86m, got {}",
        alt
    );
}

#[test]
fn per_target_backfill_tracking_and_junk_filter() {
    use truongsync_db::models::is_mac_metadata_path;

    assert!(is_mac_metadata_path(".DS_Store"));
    assert!(is_mac_metadata_path("2026/Hikes/._IMG_0001.CR3"));
    assert!(is_mac_metadata_path(".Trashes/501/IMG_0001.JPG"));
    assert!(is_mac_metadata_path("$RECYCLE.BIN/S-1-5-21/IMG.JPG"));
    assert!(is_mac_metadata_path(
        "System Volume Information/IndexerVolumeGuid"
    ));
    assert!(is_mac_metadata_path("2026/Hikes/Thumbs.db"));
    assert!(is_mac_metadata_path("2026/Hikes/desktop.ini"));
    assert!(!is_mac_metadata_path("2026/Hikes/RAW/IMG_0001.CR3"));
    assert!(!is_mac_metadata_path("2026/Hikes/RAW/IMG_0001.xmp"));

    let dir = tempdir().unwrap();
    let db = DbHandle::open(&dir.path().join("state.db")).unwrap();
    let photo_path = dir.path().join("2026/Hikes/IMG_1001.JPG");

    db.record_intentional_update(
        &photo_path.to_string_lossy(),
        "aaaa1111",
        "bbbb2222",
        "gps_backfill",
    )
    .unwrap();

    let cloud_pending = db
        .list_pending_intentional_updates_for_target("cloud", "cloud-1")
        .unwrap();
    let ext_pending = db
        .list_pending_intentional_updates_for_target("external", "ext-1")
        .unwrap();
    assert_eq!(cloud_pending.len(), 1);
    assert_eq!(ext_pending.len(), 1);

    // Mark applied for cloud-1 ONLY — ext-1 must STILL see it pending!
    db.mark_intentional_updates_applied(&[cloud_pending[0].id.clone()], "cloud", "cloud-1")
        .unwrap();
    assert!(db
        .list_pending_intentional_updates_for_target("cloud", "cloud-1")
        .unwrap()
        .is_empty());
    assert_eq!(
        db.list_pending_intentional_updates_for_target("external", "ext-1")
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn round2_utf8_paths_corrupt_cr3_gpx_entities_and_remote_validation() {
    use std::path::Path;
    use truongsync_db::models::relative_normalized;
    use truongsync_geo::gpx::parse_gpx_str;
    use truongsync_geo::matcher::cr3_has_gps_box;
    use truongsync_sync::cloud::validate_rclone_remote_name;

    // 1. UTF-8 byte-boundary slicing regression (must NEVER panic!)
    let root = Path::new("/Users/ian/Photos");
    assert_eq!(
        relative_normalized(root, Path::new("/Users/ian/写真/IMG_0001.JPG")),
        None
    );
    assert_eq!(
        relative_normalized(root, Path::new("/Users/ian/Photos/Café/IMG_0001.JPG")),
        Some("Café/IMG_0001.JPG".into())
    );
    assert_eq!(
        relative_normalized(root, Path::new("Users/ian/Photos/IMG_0001.JPG")),
        None
    );

    // 2. Corrupt CR3 box with size32=1, largesize=u64::MAX must NOT hang or panic
    let dir = tempdir().unwrap();
    let corrupt_cr3 = dir.path().join("corrupt.cr3");
    let mut bytes = vec![0, 0, 0, 1, b'f', b'r', b'e', b'e'];
    bytes.extend_from_slice(&u64::MAX.to_be_bytes());
    std::fs::write(&corrupt_cr3, &bytes).unwrap();
    assert!(!cr3_has_gps_box(&corrupt_cr3));

    // 3. GPX character reference (`&#45;`) + fractional compact offset & unqualified UTC timestamp
    let xml = r#"<gpx version="1.1"><trk><trkseg>
        <trkpt lat="36.2" lon="-116.8"><ele>&#45;85.5</ele><time>2026-11-01T22:00:00.500+0000</time></trkpt>
        <trkpt lat="36.3" lon="-116.9"><ele>10.0</ele><time>2026-11-01T22:05:00</time></trkpt>
    </trkseg></trk></gpx>"#;
    let segs = parse_gpx_str(xml).unwrap();
    assert_eq!(segs.len(), 1);
    assert_eq!(segs[0].points.len(), 2);
    assert_eq!(segs[0].points[0].ele, Some(-85.5));

    // 4. rclone remote name validation blocks flag injection
    assert!(validate_rclone_remote_name("--config").is_err());
    assert!(validate_rclone_remote_name("bad remote").is_err());
    assert_eq!(
        validate_rclone_remote_name("rawphotos:").unwrap(),
        "rawphotos"
    );
}

#[test]
fn round2_external_sync_immutability_disconnected_guard_and_backfill() {
    use truongsync_geo::exiftool::compute_file_md5;
    use truongsync_sync::external::{sync_external_roots, verify_external_roots};

    let dir = tempdir().unwrap();
    let db = DbHandle::open(&dir.path().join("state.db")).unwrap();
    let settings = AppSettings::default();
    let cancel = CancellationToken::new();

    let missing_local = dir.path().join("unmounted_local");
    let ext_root = dir.path().join("ext_drive/Backup");
    std::fs::create_dir_all(&ext_root).unwrap();

    let profile_id = db
        .upsert_external_profile(&ExternalProfile {
            id: "".into(),
            name: "Test SSD".into(),
            local_root: missing_local.to_string_lossy().to_string(),
            volume_uuid: "UUID-TEST".into(),
            volume_label: "Test SSD".into(),
            relative_dest_path: "Backup".into(),
            direction: "two_way_additive".into(),
            enabled: true,
            schedule_mode: "manual".into(),
            schedule_value: "".into(),
            schedule_action: "sync".into(),
            write_marker_file: true,
            last_sync_at: None,
            last_verify_at: None,
            last_status: "never".into(),
            last_error: None,
            pending_push_count: 0,
            pending_pull_count: 0,
            pending_bytes: 0,
            stale_after_days: 14,
        })
        .unwrap();
    let mut profile = db.get_external_profile(&profile_id).unwrap().unwrap();

    // 1. Non-restore sync MUST refuse when local_root does not exist and must NOT create it!
    let err = sync_external_roots(
        &profile,
        &missing_local,
        &ext_root,
        true,
        Some(1024 * 1024 * 1024),
        &db,
        &settings,
        false,
        &cancel,
        |_, _, _, _| {},
    );
    assert!(err.is_err());
    assert!(!missing_local.exists());

    // 2. Real 2-way additive sync with immutable conflict + push + pull + verified GPS backfill
    let local_root = dir.path().join("local_lib");
    std::fs::create_dir_all(&local_root).unwrap();
    profile.local_root = local_root.to_string_lossy().to_string();
    db.upsert_external_profile(&profile).unwrap();

    std::fs::write(local_root.join("conflict.jpg"), b"local-version-111").unwrap();
    std::fs::write(ext_root.join("conflict.jpg"), b"ext-version-22222222").unwrap();
    std::fs::write(local_root.join("push_me.jpg"), b"new-local-photo").unwrap();
    std::fs::write(ext_root.join("pull_me.jpg"), b"travel-ext-photo").unwrap();

    // Verified GPS backfill file
    let bf_ext = ext_root.join("backfill.jpg");
    let bf_loc = local_root.join("backfill.jpg");
    std::fs::write(&bf_ext, b"pre-geotag-bytes").unwrap();
    let pre_md5 = compute_file_md5(&bf_ext).unwrap();
    std::fs::write(&bf_loc, b"post-geotag-bytes-with-gps").unwrap();
    let post_md5 = compute_file_md5(&bf_loc).unwrap();
    db.record_intentional_update(
        &bf_loc.to_string_lossy(),
        &pre_md5,
        &post_md5,
        "gps_backfill",
    )
    .unwrap();

    let rep = verify_external_roots(&profile, &local_root, &ext_root, &db, false, false, &cancel)
        .unwrap();
    assert_eq!(rep.conflicts.len(), 1);
    assert_eq!(rep.to_push.len(), 1);
    assert_eq!(rep.to_pull.len(), 1);
    assert_eq!(rep.backfill_updates.len(), 1);

    let res = sync_external_roots(
        &profile,
        &local_root,
        &ext_root,
        true,
        Some(1024 * 1024 * 1024),
        &db,
        &settings,
        false,
        &cancel,
        |_, _, _, _| {},
    )
    .unwrap();
    assert_eq!(res.pushed_count, 1);
    assert_eq!(res.pulled_count, 1);
    assert_eq!(res.backfill_replaced_count, 1);
    assert_eq!(res.failed_count, 0);

    // Conflict untouched on BOTH sides!
    assert_eq!(
        std::fs::read(local_root.join("conflict.jpg")).unwrap(),
        b"local-version-111"
    );
    assert_eq!(
        std::fs::read(ext_root.join("conflict.jpg")).unwrap(),
        b"ext-version-22222222"
    );
    // Push, pull, and verified backfill all succeeded!
    assert_eq!(
        std::fs::read(ext_root.join("push_me.jpg")).unwrap(),
        b"new-local-photo"
    );
    assert_eq!(
        std::fs::read(local_root.join("pull_me.jpg")).unwrap(),
        b"travel-ext-photo"
    );
    assert_eq!(
        std::fs::read(&bf_ext).unwrap(),
        b"post-geotag-bytes-with-gps"
    );
}
