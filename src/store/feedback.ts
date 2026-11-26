import { create } from 'zustand';

export type ToastTone = 'info' | 'success' | 'error';

export interface Toast {
  id: number;
  message: string;
  tone: ToastTone;
}

export interface ConfirmRequest {
  id: number;
  title: string;
  message?: string;
  confirmLabel: string;
  resolve: (ok: boolean) => void;
}

export const TOAST_MS = 4000;
export const TOAST_ERROR_MS = 8000;

interface FeedbackState {
  toasts: Toast[];
  confirm: ConfirmRequest | null;
  push: (toast: Omit<Toast, 'id' | 'tone'> & { tone?: ToastTone }) => number;
  dismiss: (id: number) => void;
  ask: (request: Omit<ConfirmRequest, 'id' | 'resolve'>) => Promise<boolean>;
  answer: (ok: boolean) => void;
  clearAll: () => void;
}

let nextId = 1;

export const useFeedback = create<FeedbackState>((set, get) => ({
  toasts: [],
  confirm: null,
  push: (item) => {
    const id = nextId++;
    set((s) => ({
      toasts: [...s.toasts, { ...item, tone: item.tone ?? 'info', id }].slice(-3),
    }));
    return id;
  },
  dismiss: (id) => set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })),
  ask: (request) =>
    new Promise<boolean>((resolve) => {
      get().confirm?.resolve(false);
      set({ confirm: { ...request, id: nextId++, resolve } });
    }),
  answer: (ok) => {
    const cur = get().confirm;
    set({ confirm: null });
    cur?.resolve(ok);
  },
  clearAll: () => {
    const cur = get().confirm;
    set({ toasts: [], confirm: null });
    cur?.resolve(false);
  },
}));

const errText = (err: unknown): string => (err instanceof Error ? err.message : String(err));

export const notifyInfo = (message: string) =>
  useFeedback.getState().push({ message, tone: 'info' });
export const notifySuccess = (message: string) =>
  useFeedback.getState().push({ message, tone: 'success' });
export const notifyError = (err: unknown, prefix?: string) =>
  useFeedback
    .getState()
    .push({ message: prefix ? `${prefix}: ${errText(err)}` : errText(err), tone: 'error' });

export const confirmAction = (
  title: string,
  confirmLabel: string,
  message?: string,
): Promise<boolean> => useFeedback.getState().ask({ title, confirmLabel, message });
