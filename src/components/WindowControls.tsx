import React, { useEffect, useState } from 'react';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { isTauri } from '../lib/tauri';

export const isWindowsPlatform = (): boolean =>
  typeof navigator !== 'undefined' && /Win/.test(navigator.platform);

type AppWindow = ReturnType<typeof getCurrentWindow>;
const run = (fn: (w: AppWindow) => Promise<unknown>) => () => {
  if (!isTauri()) return;
  try {
    void fn(getCurrentWindow()).catch(() => {});
  } catch {
    // ignore outside Tauri
  }
};

const Glyph: React.FC<{ kind: 'minimize' | 'maximize' | 'restore' | 'close' }> = ({ kind }) => (
  <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true" fill="none" stroke="currentColor" strokeWidth="1" shapeRendering="crispEdges">
    {kind === 'minimize' && <path d="M0 5.5h10" />}
    {kind === 'maximize' && <rect x="0.5" y="0.5" width="9" height="9" rx="1" />}
    {kind === 'restore' && (
      <>
        <rect x="0.5" y="2.5" width="7" height="7" rx="1" />
        <path d="M2.5 2.5V1.5a1 1 0 0 1 1-1h5a1 1 0 0 1 1 1v5a1 1 0 0 1-1 1h-1" />
      </>
    )}
    {kind === 'close' && <path d="M0.5 0.5l9 9M9.5 0.5l-9 9" shapeRendering="geometricPrecision" />}
  </svg>
);

let sharedMaximized = false;
const listeners = new Set<(m: boolean) => void>();
let initialized = false;

export function ensureWindowStateWatcher(): void {
  if (initialized || !isTauri()) return;
  initialized = true;
  const sync = () => {
    try {
      const win = getCurrentWindow();
      void Promise.all([
        win.isMaximized().catch(() => false),
        win.isFullscreen().catch(() => false),
      ]).then(([m, fs]) => {
        sharedMaximized = m;
        document.documentElement.classList.toggle('is-maximized', m);
        document.documentElement.classList.toggle('is-fullscreen', fs);
        listeners.forEach((fn) => fn(m));
      });
    } catch {
      // ignore
    }
  };
  sync();
  try {
    void getCurrentWindow().onResized(sync).catch(() => {});
  } catch {
    // ignore
  }
}

export const WindowsCaptionButtons: React.FC = () => {
  const [maximized, setMaximized] = useState(sharedMaximized);
  useEffect(() => {
    if (!isTauri()) return;
    ensureWindowStateWatcher();
    listeners.add(setMaximized);
    return () => {
      listeners.delete(setMaximized);
    };
  }, []);
  return (
    <div className="caption-buttons">
      <button type="button" className="caption-button" aria-label="Minimize" onClick={run((w) => w.minimize())}>
        <Glyph kind="minimize" />
      </button>
      <button
        type="button"
        className="caption-button"
        aria-label={maximized ? 'Restore' : 'Maximize'}
        onClick={run(async (w) => {
          await w.toggleMaximize();
          const m = await w.isMaximized();
          sharedMaximized = m;
          setMaximized(m);
        })}
      >
        <Glyph kind={maximized ? 'restore' : 'maximize'} />
      </button>
      <button type="button" className="caption-button caption-close" aria-label="Close" onClick={run((w) => w.hide())}>
        <Glyph kind="close" />
      </button>
    </div>
  );
};
