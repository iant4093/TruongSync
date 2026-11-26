import { create } from 'zustand';
import { listen } from '@tauri-apps/api/event';
import { api, isTauri } from '../lib/tauri';
import type {
  AppSettings,
  BootstrapInfo,
  CloudProfile,
  DetectedVolume,
  ExternalProfile,
  ImportPreset,
  JobProgressEvent,
  JobRecord,
  NavTab,
  ToolsStatus,
} from '../types/api';
import { notifyError, notifyInfo } from './feedback';

interface SyncStore {
  activeNav: NavTab;
  bootstrapped: boolean;
  bootstrap: BootstrapInfo | null;
  tools: ToolsStatus | null;
  settings: AppSettings | null;
  volumes: DetectedVolume[];
  presets: ImportPreset[];
  cloudProfiles: CloudProfile[];
  externalProfiles: ExternalProfile[];
  jobs: JobRecord[];
  activeJobs: Record<string, JobProgressEvent>;
  activeProgress: JobProgressEvent | null;
  pendingAutoImportPath: string | null;
  inspectorJobId: string | null;

  setActiveNav: (tab: NavTab) => void;
  setActiveProgress: (p: JobProgressEvent | null) => void;
  setPendingAutoImportPath: (path: string | null) => void;
  setInspectorJobId: (id: string | null) => void;

  init: () => Promise<void>;
  refreshVolumes: () => Promise<void>;
  refreshProfiles: () => Promise<void>;
  refreshJobs: () => Promise<void>;
  refreshTools: () => Promise<void>;
  refreshSettings: () => Promise<void>;
}

let listenersAttached = false;

function latestProgress(map: Record<string, JobProgressEvent>): JobProgressEvent | null {
  const vals = Object.values(map);
  return vals.length > 0 ? vals[vals.length - 1] : null;
}

export const useSyncStore = create<SyncStore>((set, get) => ({
  activeNav: 'import',
  bootstrapped: false,
  bootstrap: null,
  tools: null,
  settings: null,
  volumes: [],
  presets: [],
  cloudProfiles: [],
  externalProfiles: [],
  jobs: [],
  activeJobs: {},
  activeProgress: null,
  pendingAutoImportPath: null,
  inspectorJobId: null,

  setActiveNav: (tab) =>
    set((state) => ({
      activeNav: tab,
      inspectorJobId: tab === 'activity' ? state.inspectorJobId : null,
    })),
  setActiveProgress: (p) =>
    set((state) => {
      if (!p) {
        return { activeJobs: {}, activeProgress: null };
      }
      const next = { ...state.activeJobs, [p.job_id]: p };
      return { activeJobs: next, activeProgress: p };
    }),
  setPendingAutoImportPath: (path) => set({ pendingAutoImportPath: path }),
  setInspectorJobId: (id) => set({ inspectorJobId: id }),

  init: async () => {
    if (!isTauri()) {
      set({ bootstrapped: true });
      return;
    }

    // Attach event listeners FIRST (idempotently) so a single bootstrap IPC error never bricks events!
    if (!listenersAttached) {
      listenersAttached = true;
      void listen<JobProgressEvent>('job-progress', (ev) => {
        set((state) => {
          const next = { ...state.activeJobs, [ev.payload.job_id]: ev.payload };
          return { activeJobs: next, activeProgress: ev.payload };
        });
      });
      void listen<DetectedVolume[]>('volumes-changed', (ev) => {
        set({ volumes: ev.payload });
      });
      void listen<NavTab>('navigate-tab', (ev) => {
        get().setActiveNav(ev.payload);
      });
      void listen<DetectedVolume>('auto-import-volume', (ev) => {
        get().setActiveNav('import');
        set({ pendingAutoImportPath: ev.payload.mount_path });
        notifyInfo(`SD card detected: ${ev.payload.label}`);
      });
      void listen<string>('auto-import-volume-path', (ev) => {
        get().setActiveNav('import');
        set({ pendingAutoImportPath: ev.payload });
      });
      void listen<{ profile_name: string; volume_label: string }>('backup-drive-connected', (ev) => {
        notifyInfo(`Backup drive connected: ${ev.payload.volume_label} (${ev.payload.profile_name})`);
      });
      void listen('profiles-updated', () => {
        void get().refreshProfiles();
      });
      void listen<string | null>('jobs-updated', (ev) => {
        set((state) => {
          const finishedId = ev.payload;
          if (finishedId && state.activeJobs[finishedId]) {
            const next = { ...state.activeJobs };
            delete next[finishedId];
            return { activeJobs: next, activeProgress: latestProgress(next) };
          }
          if (!finishedId) {
            return { activeJobs: {}, activeProgress: null };
          }
          return state;
        });
        void get().refreshJobs();
      });
      void listen<boolean>('battery-changed', (ev) => {
        set((state) =>
          state.bootstrap
            ? { bootstrap: { ...state.bootstrap, on_battery: ev.payload } }
            : state,
        );
      });
    }

    const results = await Promise.allSettled([
      api.getBootstrapInfo(),
      api.checkTools(),
      api.getSettings(),
      api.listVolumes(),
      api.listImportPresets(),
      api.listCloudProfiles(),
      api.listExternalProfiles(),
      api.listJobs(100),
    ]);

    const patch: Partial<SyncStore> = { bootstrapped: true };
    if (results[0].status === 'fulfilled') {
      patch.bootstrap = results[0].value;
      patch.pendingAutoImportPath = results[0].value.initial_import_volume;
    }
    if (results[1].status === 'fulfilled') patch.tools = results[1].value;
    if (results[2].status === 'fulfilled') patch.settings = results[2].value;
    if (results[3].status === 'fulfilled') patch.volumes = results[3].value;
    if (results[4].status === 'fulfilled') patch.presets = results[4].value;
    if (results[5].status === 'fulfilled') patch.cloudProfiles = results[5].value;
    if (results[6].status === 'fulfilled') patch.externalProfiles = results[6].value;
    if (results[7].status === 'fulfilled') patch.jobs = results[7].value;

    set(patch);

    const firstFailure = results.find((r) => r.status === 'rejected');
    if (firstFailure && firstFailure.status === 'rejected') {
      notifyError(firstFailure.reason, 'Some startup data failed to load');
    }
  },

  refreshVolumes: async () => {
    if (!isTauri()) return;
    try {
      const volumes = await api.listVolumes();
      set({ volumes });
    } catch {
      // ignore transient volume poll error
    }
  },

  refreshProfiles: async () => {
    if (!isTauri()) return;
    const [cloudRes, extRes, presetRes] = await Promise.allSettled([
      api.listCloudProfiles(),
      api.listExternalProfiles(),
      api.listImportPresets(),
    ]);
    const patch: Partial<SyncStore> = {};
    if (cloudRes.status === 'fulfilled') patch.cloudProfiles = cloudRes.value;
    if (extRes.status === 'fulfilled') patch.externalProfiles = extRes.value;
    if (presetRes.status === 'fulfilled') patch.presets = presetRes.value;
    set(patch);
  },

  refreshJobs: async () => {
    if (!isTauri()) return;
    try {
      const jobs = await api.listJobs(100);
      set({ jobs });
    } catch {
      // ignore
    }
  },

  refreshTools: async () => {
    if (!isTauri()) return;
    try {
      const tools = await api.checkTools();
      set({ tools });
    } catch {
      // ignore
    }
  },

  refreshSettings: async () => {
    if (!isTauri()) return;
    try {
      const settings = await api.getSettings();
      set({ settings });
    } catch {
      // ignore
    }
  },
}));
