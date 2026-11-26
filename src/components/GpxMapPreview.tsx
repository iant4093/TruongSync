import React, { useEffect, useRef, useState } from 'react';
import * as maplibregl from 'maplibre-gl';
import maplibreWorkerUrl from 'maplibre-gl/dist/maplibre-gl-worker.mjs?url';
import 'maplibre-gl/dist/maplibre-gl.css';
import type { GeotagPlan } from '../types/api';

let workerConfigured = false;
function ensureMapLibreWorker() {
  if (workerConfigured) return;
  workerConfigured = true;
  if (typeof maplibreWorkerUrl === 'string' && maplibreWorkerUrl.length > 0) {
    maplibregl.setWorkerUrl(maplibreWorkerUrl);
  }
}

const DARK_STYLE_URL = 'https://tiles.openfreemap.org/styles/dark';

const OFFLINE_DARK_STYLE: maplibregl.StyleSpecification = {
  version: 8,
  sources: {},
  layers: [
    {
      id: 'offline-bg',
      type: 'background',
      paint: { 'background-color': '#171717' },
    },
  ],
};

function buildGeoJson(plan: GeotagPlan) {
  const lineFeatures = plan.preview_lines
    .filter((line) => line.length >= 2)
    .map((line) => ({
      type: 'Feature' as const,
      properties: { kind: 'track' },
      geometry: { type: 'LineString' as const, coordinates: line },
    }));

  const photoFeatures = plan.items
    .filter((i) => i.status === 'matched' && i.lat != null && i.lon != null)
    .map((i) => ({
      type: 'Feature' as const,
      properties: { name: i.filename },
      geometry: { type: 'Point' as const, coordinates: [i.lon!, i.lat!] as [number, number] },
    }));

  return { lineFeatures, photoFeatures };
}

function applyPlanToMap(map: maplibregl.Map, plan: GeotagPlan) {
  const { lineFeatures, photoFeatures } = buildGeoJson(plan);
  const trackData = {
    type: 'FeatureCollection' as const,
    features: lineFeatures,
  };
  const photoData = {
    type: 'FeatureCollection' as const,
    features: photoFeatures,
  };

  const trackSource = map.getSource('gpx-track') as maplibregl.GeoJSONSource | undefined;
  if (trackSource) {
    trackSource.setData(trackData);
  } else {
    map.addSource('gpx-track', { type: 'geojson', data: trackData });
    map.addLayer({
      id: 'gpx-track-line',
      type: 'line',
      source: 'gpx-track',
      paint: {
        'line-color': '#8fd19e',
        'line-width': 3,
        'line-opacity': 0.92,
      },
    });
  }

  const photoSource = map.getSource('gpx-photos') as maplibregl.GeoJSONSource | undefined;
  if (photoSource) {
    photoSource.setData(photoData);
  } else {
    map.addSource('gpx-photos', { type: 'geojson', data: photoData });
    map.addLayer({
      id: 'gpx-photo-dots',
      type: 'circle',
      source: 'gpx-photos',
      paint: {
        'circle-radius': 5,
        'circle-color': '#ededed',
        'circle-stroke-width': 2,
        'circle-stroke-color': '#171717',
      },
    });
  }

  const bounds = new maplibregl.LngLatBounds();
  let hasCoords = false;
  for (const line of plan.preview_lines) {
    for (const pt of line) {
      bounds.extend(pt);
      hasCoords = true;
    }
  }
  for (const f of photoFeatures) {
    bounds.extend(f.geometry.coordinates);
    hasCoords = true;
  }
  if (hasCoords) {
    map.fitBounds(bounds, { padding: 48, maxZoom: 14, duration: 0 });
  }
}

export const GpxMapPreview: React.FC<{ plan: GeotagPlan }> = ({ plan }) => {
  const containerRef = useRef<HTMLDivElement>(null);
  const mapRef = useRef<maplibregl.Map | null>(null);
  const planRef = useRef<GeotagPlan>(plan);
  const loadedRef = useRef(false);
  const fellBackRef = useRef(false);
  const [offlineMode, setOfflineMode] = useState(false);
  const [webglError, setWebglError] = useState(false);

  planRef.current = plan;

  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;

    loadedRef.current = false;
    fellBackRef.current = false;
    setWebglError(false);

    let map: maplibregl.Map;
    try {
      ensureMapLibreWorker();
      map = new maplibregl.Map({
        container: el,
        style: DARK_STYLE_URL,
        center: [-121.7, 45.37],
        zoom: 9,
        attributionControl: false,
      });
    } catch {
      setWebglError(true);
      return;
    }
    mapRef.current = map;

    const syncLayers = () => {
      loadedRef.current = true;
      try {
        applyPlanToMap(map, planRef.current);
      } catch {
        // Style might still be swapping
      }
    };

    map.on('style.load', syncLayers);
    const resizeObserver =
      typeof ResizeObserver !== 'undefined'
        ? new ResizeObserver(() => {
            if (el.offsetWidth > 0 && el.offsetHeight > 0) {
              map.resize();
            }
          })
        : null;
    resizeObserver?.observe(el);
    map.on('error', (ev) => {
      // Only fall back to blank dark style when the base style itself fails to load (offline/unreachable)
      const isStyleFailure =
        !loadedRef.current &&
        (ev.error?.message?.toLowerCase().includes('fetch') ||
          ev.error?.message?.toLowerCase().includes('network') ||
          ev.error?.message?.toLowerCase().includes('style') ||
          !map.isStyleLoaded());
      if (!fellBackRef.current && isStyleFailure) {
        fellBackRef.current = true;
        setOfflineMode(true);
        map.setStyle(OFFLINE_DARK_STYLE);
      }
    });

    return () => {
      resizeObserver?.disconnect();
      map.remove();
      mapRef.current = null;
      loadedRef.current = false;
    };
  }, []);

  useEffect(() => {
    const map = mapRef.current;
    if (!map || !loadedRef.current) return;
    try {
      applyPlanToMap(map, plan);
    } catch {
      // Wait for style.load
    }
  }, [plan]);

  return (
    <div className="map-view-shell">
      <div className="map-status-pill">
        <span>{plan.track_point_count.toLocaleString()} GPX pts</span>
        <span>·</span>
        <span style={{ color: 'var(--ok)' }}>{plan.matched_count} matched</span>
        {plan.out_of_range_count > 0 && (
          <>
            <span>·</span>
            <span style={{ color: 'var(--warn-text)' }}>{plan.out_of_range_count} out of range</span>
          </>
        )}
        {plan.skipped_video_count > 0 && (
          <>
            <span>·</span>
            <span className="muted">{plan.skipped_video_count} videos skipped</span>
          </>
        )}
        {offlineMode && (
          <>
            <span>·</span>
            <span className="muted">Offline — showing GPX track without base map</span>
          </>
        )}
      </div>
      {webglError ? (
        <div
          className="map-canvas muted"
          style={{ display: 'grid', placeItems: 'center', fontSize: 12, padding: 16 }}
        >
          Map preview unavailable (WebGL disabled on this device) — matched coordinates are listed below.
        </div>
      ) : (
        <div ref={containerRef} className="map-canvas" />
      )}
    </div>
  );
};
