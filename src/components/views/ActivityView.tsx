import React, { useEffect, useState } from 'react';
import { Activity, RefreshCw, X } from 'lucide-react';
import { isTopEscapeLayer, popEscapeLayer, pushEscapeLayer } from '../../lib/escapeStack';
import { api, formatBytes, formatRelativeAge } from '../../lib/tauri';
import { useSyncStore } from '../../store/useSyncStore';
import type { JobItemRecord } from '../../types/api';
import { Badge, EmptyState, SectionTitle, SegmentedControl, ViewHeader } from '../ui';

export const ActivityView: React.FC = () => {
  const jobs = useSyncStore((s) => s.jobs);
  const activeProgress = useSyncStore((s) => s.activeProgress);
  const refreshJobs = useSyncStore((s) => s.refreshJobs);
  const inspectorJobId = useSyncStore((s) => s.inspectorJobId);
  const setInspectorJobId = useSyncStore((s) => s.setInspectorJobId);

  const [filter, setFilter] = useState<'all' | 'pipeline' | 'geotag' | 'cloud' | 'external'>('all');

  const filtered = jobs.filter((j) => {
    if (filter === 'all') return true;
    if (filter === 'cloud') return j.kind.startsWith('cloud');
    if (filter === 'external') return j.kind.startsWith('external');
    return j.kind === filter;
  });

  return (
    <div className="view-content">
      <ViewHeader
        title="Activity & Job History"
        subtitle="Live transfer progress, throughput, and persistent SQLite audit log across all imports, geotag batches, cloud syncs, and external backups."
        actions={
          <button type="button" className="button" onClick={() => void refreshJobs()}>
            <RefreshCw size={14} /> Refresh
          </button>
        }
      />

      {activeProgress && (
        <div className="card" style={{ marginBottom: 16 }}>
          <div className="card-body stack">
            <div className="row-between">
              <div className="row">
                <Badge tone="info">{activeProgress.stage.toUpperCase()}</Badge>
                <strong>{activeProgress.message}</strong>
              </div>
              <span className="mono muted tabular">
                {activeProgress.speed_bps > 0 ? `${formatBytes(activeProgress.speed_bps)}/s` : ''}
              </span>
            </div>
            <div className="upload-progress-bar">
              <div
                style={{
                  width: `${
                    activeProgress.files_total > 0
                      ? Math.min(100, (activeProgress.files_done / activeProgress.files_total) * 100)
                      : 20
                  }%`,
                }}
              />
            </div>
            <div className="row-between">
              <span className="muted tabular">
                {activeProgress.files_done} / {activeProgress.files_total}
              </span>
              <button
                type="button"
                className="button button-danger"
                onClick={() => void api.cancelActiveJob(activeProgress.job_id)}
              >
                Cancel Job
              </button>
            </div>
          </div>
        </div>
      )}

      <div className="row-between" style={{ marginBottom: 12 }}>
        <SegmentedControl
          value={filter}
          options={[
            ['all', 'All'],
            ['pipeline', 'Imports'],
            ['geotag', 'Geotag'],
            ['cloud', 'Cloud Sync'],
            ['external', 'External Backup'],
          ]}
          onChange={setFilter}
        />
        <span className="muted tabular">{filtered.length} run(s)</span>
      </div>

      {filtered.length === 0 ? (
        <EmptyState
          icon={<Activity size={26} strokeWidth={1.4} />}
          title="No job history yet"
          description="Completed imports, geotagging batches, Google Drive syncs, and external drive backups will appear here."
        />
      ) : (
        <div className="card">
          {filtered.map((j) => {
            const tone =
              j.status === 'succeeded'
                ? 'ok'
                : j.status === 'failed'
                  ? 'danger'
                  : j.status === 'running'
                    ? 'info'
                    : 'warn';
            return (
              <div
                key={j.id}
                className="folder-row"
                style={{
                  cursor: 'pointer',
                  background: inspectorJobId === j.id ? '#2b2b2b' : undefined,
                }}
                onClick={() => setInspectorJobId(j.id)}
              >
                <div className="stack" style={{ gap: 3 }}>
                  <div className="row-wrap">
                    <strong>{j.title}</strong>
                    <Badge tone={tone}>{j.status}</Badge>
                    <Badge>{j.kind}</Badge>
                  </div>
                  <div className="muted tabular">
                    {formatRelativeAge(j.started_at).label} · {j.files_done} done
                    {j.files_skipped > 0 ? ` · ${j.files_skipped} skipped` : ''}
                    {j.files_failed > 0 ? ` · ${j.files_failed} failed` : ''}
                    {j.bytes_done > 0 ? ` · ${formatBytes(j.bytes_done)}` : ''}
                  </div>
                </div>
                <span className="muted">Inspect →</span>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
};

export const JobInspectorDrawer: React.FC = () => {
  const activeNav = useSyncStore((s) => s.activeNav);
  const jobs = useSyncStore((s) => s.jobs);
  const inspectorJobId = useSyncStore((s) => s.inspectorJobId);
  const setInspectorJobId = useSyncStore((s) => s.setInspectorJobId);
  const [items, setItems] = useState<JobItemRecord[]>([]);

  const selectedJob =
    activeNav === 'activity' ? (jobs.find((j) => j.id === inspectorJobId) ?? null) : null;

  useEffect(() => {
    if (!inspectorJobId) {
      setItems([]);
      return;
    }
    let cancelled = false;
    void api
      .listJobItems(inspectorJobId, 300)
      .then((res) => {
        if (!cancelled) setItems(res);
      })
      .catch(() => {
        if (!cancelled) setItems([]);
      });
    return () => {
      cancelled = true;
    };
  }, [inspectorJobId]);

  useEffect(() => {
    if (!selectedJob) return;
    const layer = pushEscapeLayer();
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape' && isTopEscapeLayer(layer)) {
        event.preventDefault();
        setInspectorJobId(null);
      }
    };
    document.addEventListener('keydown', onKey, true);
    return () => {
      document.removeEventListener('keydown', onKey, true);
      popEscapeLayer(layer);
    };
  }, [selectedJob, setInspectorJobId]);

  return (
    <div className="inspector-slot" aria-hidden={!selectedJob}>
      {selectedJob && (
        <div className="inspector-drawer">
          <div className="inspector-heading">
            <span>Job Details</span>
            <button
              type="button"
              className="icon-button"
              aria-label="Close job inspector"
              onClick={() => setInspectorJobId(null)}
            >
              <X size={14} />
            </button>
          </div>
          <div className="card-body stack-md">
            <div>
              <h3 style={{ margin: '0 0 6px', fontSize: 15 }}>{selectedJob.title}</h3>
              <div className="row-wrap">
                <Badge
                  tone={
                    selectedJob.status === 'succeeded'
                      ? 'ok'
                      : selectedJob.status === 'failed'
                        ? 'danger'
                        : 'warn'
                  }
                >
                  {selectedJob.status}
                </Badge>
                <Badge>{selectedJob.kind}</Badge>
              </div>
            </div>
            <div className="stack muted tabular">
              <div>Started: {selectedJob.started_at}</div>
              {selectedJob.finished_at && <div>Finished: {selectedJob.finished_at}</div>}
              <div>
                Files: {selectedJob.files_done} done / {selectedJob.files_skipped} skipped /{' '}
                {selectedJob.files_failed} failed
              </div>
              {selectedJob.bytes_done > 0 && (
                <div>Transferred: {formatBytes(selectedJob.bytes_done)}</div>
              )}
            </div>
            {selectedJob.error_message && (
              <div className="error-banner" style={{ marginBottom: 0 }}>
                {selectedJob.error_message}
              </div>
            )}
            {items.length > 0 && (
              <div>
                <SectionTitle>Recorded Items ({items.length})</SectionTitle>
                <div className="stack">
                  {items.map((it) => (
                    <div key={it.id} className="panel" style={{ padding: 8, fontSize: 11 }}>
                      <div className="row-between">
                        <Badge tone={it.status === 'failed' ? 'danger' : 'ok'}>{it.status}</Badge>
                        <span className="muted">{it.stage}</span>
                      </div>
                      <div className="mono" style={{ marginTop: 4, wordBreak: 'break-all' }}>
                        {it.source_path}
                      </div>
                      {it.message && (
                        <div style={{ color: 'var(--danger-text)', marginTop: 3 }}>{it.message}</div>
                      )}
                    </div>
                  ))}
                </div>
              </div>
            )}
          </div>
        </div>
      )}
    </div>
  );
};
