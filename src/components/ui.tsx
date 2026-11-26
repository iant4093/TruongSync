import React from 'react';
import { formatRelativeAge } from '../lib/tauri';

export const ViewHeader: React.FC<{
  title: string;
  subtitle?: string;
  actions?: React.ReactNode;
}> = ({ title, subtitle, actions }) => (
  <div className="view-header">
    <div>
      <h2>{title}</h2>
      {subtitle && <div className="view-subtitle">{subtitle}</div>}
    </div>
    {actions && <div className="view-actions">{actions}</div>}
  </div>
);

export const SectionTitle: React.FC<{ children: React.ReactNode }> = ({ children }) => (
  <div className="section-title">{children}</div>
);

export type BadgeTone = 'muted' | 'warn' | 'ok' | 'danger' | 'info';

export const Badge: React.FC<{ children: React.ReactNode; tone?: BadgeTone }> = ({
  children,
  tone = 'muted',
}) => {
  const toneClass =
    tone === 'warn'
      ? 'badge-warn'
      : tone === 'ok'
        ? 'badge-ok'
        : tone === 'danger'
          ? 'badge-danger'
          : tone === 'info'
            ? 'badge-info'
            : '';
  return <span className={`badge ${toneClass}`.trim()}>{children}</span>;
};

export const FreshnessBadge: React.FC<{
  lastSyncAt: string | null | undefined;
  lastStatus: string;
  staleAfterDays: number;
}> = ({ lastSyncAt, lastStatus, staleAfterDays }) => {
  const age = formatRelativeAge(lastSyncAt);
  if (lastStatus === 'failed') {
    return <Badge tone="danger">Failed · {age.label}</Badge>;
  }
  if (lastStatus === 'needs_review') {
    return <Badge tone="warn">Review needed · {age.label}</Badge>;
  }
  if (age.days === null) {
    return <Badge tone="warn">Never synced</Badge>;
  }
  if (age.days >= staleAfterDays) {
    return <Badge tone="warn">Stale · {age.label}</Badge>;
  }
  if (lastStatus === 'behind') {
    return <Badge tone="info">Changes pending · {age.label}</Badge>;
  }
  return <Badge tone="ok">Up to date · {age.label}</Badge>;
};

export const ToggleRow: React.FC<{
  label: string;
  description?: string;
  checked: boolean;
  disabled?: boolean;
  onChange: (next: boolean) => void;
}> = ({ label, description, checked, disabled = false, onChange }) => (
  <label className="toggle-row">
    <span>
      {label}
      {description && <div className="toggle-description">{description}</div>}
    </span>
    <input
      className="toggle-switch"
      type="checkbox"
      checked={checked}
      disabled={disabled}
      onChange={(e) => onChange(e.target.checked)}
    />
  </label>
);

export function SegmentedControl<T extends string>({
  value,
  options,
  disabled = false,
  onChange,
}: {
  value: T;
  options: ReadonlyArray<readonly [T, string]>;
  disabled?: boolean;
  onChange: (next: T) => void;
}) {
  return (
    <div className="segmented-control">
      {options.map(([key, label]) => (
        <button
          key={key}
          type="button"
          className="button"
          disabled={disabled}
          aria-pressed={value === key}
          onClick={() => onChange(key)}
        >
          {label}
        </button>
      ))}
    </div>
  );
}

export const EmptyState: React.FC<{
  icon?: React.ReactNode;
  title: string;
  description?: string;
  actions?: React.ReactNode;
}> = ({ icon, title, description, actions }) => (
  <div className="empty-state">
    {icon}
    <div className="empty-state-title">{title}</div>
    {description && <div className="empty-state-description">{description}</div>}
    {actions && <div className="empty-actions">{actions}</div>}
  </div>
);
