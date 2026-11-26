import React, { useCallback, useEffect, useRef } from 'react';
import { X } from 'lucide-react';
import { isTopEscapeLayer, popEscapeLayer, pushEscapeLayer } from '../lib/escapeStack';
import { TOAST_ERROR_MS, TOAST_MS, useFeedback, type Toast } from '../store/feedback';

const ToastItem: React.FC<{ toast: Toast }> = ({ toast }) => {
  const dismiss = useFeedback((s) => s.dismiss);
  const hovered = useRef(false);
  useEffect(() => {
    let timer: number;
    const arm = () => {
      timer = window.setTimeout(() => {
        if (hovered.current) arm();
        else dismiss(toast.id);
      }, toast.tone === 'error' ? TOAST_ERROR_MS : TOAST_MS);
    };
    arm();
    return () => window.clearTimeout(timer);
  }, [toast.id, toast.tone, dismiss]);
  return (
    <div
      className={`toast toast-${toast.tone}`}
      role={toast.tone === 'error' ? 'alert' : 'status'}
      onMouseEnter={() => {
        hovered.current = true;
      }}
      onMouseLeave={() => {
        hovered.current = false;
      }}
    >
      <span className="toast-text">{toast.message}</span>
      <button
        type="button"
        className="icon-button toast-close"
        aria-label="Dismiss"
        onClick={() => dismiss(toast.id)}
      >
        <X size={14} />
      </button>
    </div>
  );
};

const ConfirmDialog: React.FC = () => {
  const request = useFeedback((s) => s.confirm);
  const answer = useFeedback((s) => s.answer);
  const cancelRef = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    if (!request) return;
    const layer = pushEscapeLayer();
    cancelRef.current?.focus();
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape' && isTopEscapeLayer(layer)) {
        event.preventDefault();
        answer(false);
      }
    };
    document.addEventListener('keydown', onKey, true);
    return () => {
      document.removeEventListener('keydown', onKey, true);
      popEscapeLayer(layer);
    };
  }, [request, answer]);

  if (!request) return null;
  return (
    <div className="modal-backdrop" onClick={() => answer(false)}>
      <div
        className="panel confirm-dialog"
        role="alertdialog"
        aria-modal="true"
        onClick={(e) => e.stopPropagation()}
      >
        <h3>{request.title}</h3>
        {request.message && <p>{request.message}</p>}
        <div className="confirm-actions">
          <button type="button" ref={cancelRef} className="button" onClick={() => answer(false)}>
            Cancel
          </button>
          <button type="button" className="button button-primary" onClick={() => answer(true)}>
            {request.confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
};

export const FeedbackHost: React.FC = () => {
  const toasts = useFeedback((s) => s.toasts);
  return (
    <>
      <div className="toast-stack" aria-live="polite">
        {toasts.map((t) => (
          <ToastItem key={t.id} toast={t} />
        ))}
      </div>
      <ConfirmDialog />
    </>
  );
};

export const StatusBanner: React.FC = () => {
  const latest = useFeedback((s) => s.toasts[s.toasts.length - 1]);
  const dismiss = useFeedback((s) => s.dismiss);
  const dismissLatest = useCallback(() => {
    if (latest) dismiss(latest.id);
  }, [latest, dismiss]);
  return (
    <div className="status-banner-slot" aria-live="polite">
      {latest && (
        <div key={latest.id} className={`status-banner status-${latest.tone}`} title={latest.message}>
          <span className="status-banner-text">{latest.message}</span>
          <button type="button" className="status-banner-close" aria-label="Dismiss" onClick={dismissLatest}>
            <X size={12} />
          </button>
        </div>
      )}
    </div>
  );
};
