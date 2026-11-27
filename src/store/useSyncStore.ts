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
  UpdateDownloadProgress,
  UpdateFeedStatus,
} from '../types/api';
import { notifyError, notifyInfo, notifySuccess } from './feedback';

interface SyncStore {
  activeNav: NavTab;
  bootstrapped: boolean;
  toolsChecked: boolean;
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
  checkingUpdates: boolean;
  updateStatus: UpdateFeedStatus | null;
  installingUpdate: boolean;
  updateProgress: UpdateDownloadProgress | null;

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
  checkForUpdates: () => Promise<void>;
  installUpdate: () => Promise<void>;
}

let listenersAttached = false;
let initInFlight: Promise<void> | null = null;
let volumesSeq = 0;
let toolsSeq = 0;
let profilesSeq = 0;
let jobsSeq = 0;
let settingsSeq = 0;

function latestProgress(map: Record<string, JobProgressEvent>): JobProgressEvent | null {
  const vals = Object.values(map);
  return vals.length > 0 ? vals[vals.length - 1] : null;
}

export const useSyncStore = create<SyncStore>((set, get) => ({
  activeNav: 'import',
  bootstrapped: false,
  toolsChecked: false,
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
  checkingUpdates: false,
  updateStatus: null,
  installingUpdate: false,
  updateProgress: null,

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
      set({ bootstrapped: true, toolsChecked: true });
      return;
    }
    if (get().bootstrapped) return;
    if (initInFlight) return initInFlight;

    initInFlight = (async () => {
      // Attach and await event listeners FIRST so bootstrap IPC never races or drops live SD/update events
      if (!listenersAttached) {
        listenersAttached = true;
        await Promise.all([
          listen<JobProgressEvent>('job-progress', (ev) => {
            set((state) => {
              const next = { ...state.activeJobs, [ev.payload.job_id]: ev.payload };
              return { activeJobs: next, activeProgress: ev.payload };
            });
          }),
          listen<DetectedVolume[]>('volumes-changed', (ev) => {
            volumesSeq += 1;
            set({ volumes: ev.payload });
          }),
          listen<NavTab>('navigate-tab', (ev) => {
            get().setActiveNav(ev.payload);
          }),
          listen<DetectedVolume>('auto-import-volume', (ev) => {
            get().setActiveNav('import');
            set({ pendingAutoImportPath: ev.payload.mount_path });
            notifyInfo(`SD card detected: ${ev.payload.label}`);
          }),
          listen<string>('auto-import-volume-path', (ev) => {
            get().setActiveNav('import');
            set({ pendingAutoImportPath: ev.payload });
          }),
          listen<{ profile_name: string; volume_label: string }>('backup-drive-connected', (ev) => {
            notifyInfo(`Backup drive connected: ${ev.payload.volume_label} (${ev.payload.profile_name})`);
          }),
          listen('profiles-updated', () => {
            void get().refreshProfiles();
          }),
          listen<string | null>('jobs-updated', (ev) => {
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
          }),
          listen<boolean>('battery-changed', (ev) => {
            set((state) =>
              state.bootstrap
                ? { bootstrap: { ...state.bootstrap, on_battery: ev.payload } }
                : state,
            );
          }),
          listen<UpdateDownloadProgress>('desktop-update-progress', (ev) => {
            set({ updateProgress: ev.payload });
          }),
        ]).catch(() => {});
      }

      // Fire slow OS probes (exiftool/rclone subprocesses & diskutil volume scan) concurrently
      // WITHOUT blocking first paint on fast SQLite queries, guarded by monotonic sequence IDs
      // so stale startup responses never overwrite newer live events or manual refreshes.
      const initToolsSeq = ++toolsSeq;
      const toolsPromise = api
        .checkTools()
        .then((tools) => {
          if (initToolsSeq === toolsSeq) {
            set({ tools, toolsChecked: true });
          }
        })
        .catch(() => {
          if (initToolsSeq === toolsSeq) {
            set({ toolsChecked: true });
          }
        });
      const initVolumesSeq = ++volumesSeq;
      const volumesPromise = api
        .listVolumes()
        .then((volumes) => {
          if (initVolumesSeq === volumesSeq) {
            set({ volumes });
          }
        })
        .catch(() => {});

      const initSettingsSeq = ++settingsSeq;
      const initProfilesSeq = ++profilesSeq;
      const initJobsSeq = ++jobsSeq;

      const fastResults = await Promise.allSettled([
        api.getBootstrapInfo(),
        api.getSettings(),
        api.listImportPresets(),
        api.listCloudProfiles(),
        api.listExternalProfiles(),
        api.listJobs(100),
      ]);

      const initialVol =
        fastResults[0].status === 'fulfilled' ? fastResults[0].value.initial_import_volume : null;
      const patch: Partial<SyncStore> = { bootstrapped: true };
      if (fastResults[0].status === 'fulfilled') patch.bootstrap = fastResults[0].value;
      if (fastResults[1].status === 'fulfilled' && initSettingsSeq === settingsSeq) {
        patch.settings = fastResults[1].value;
      }
      if (initProfilesSeq === profilesSeq) {
        if (fastResults[2].status === 'fulfilled') patch.presets = fastResults[2].value;
        if (fastResults[3].status === 'fulfilled') patch.cloudProfiles = fastResults[3].value;
        if (fastResults[4].status === 'fulfilled') patch.externalProfiles = fastResults[4].value;
      }
      if (fastResults[5].status === 'fulfilled' && initJobsSeq === jobsSeq) {
        patch.jobs = fastResults[5].value;
      }

      set((state) => ({
        ...patch,
        pendingAutoImportPath: state.pendingAutoImportPath ?? initialVol,
      }));

      const firstFailure = fastResults.find((r) => r.status === 'rejected');
      if (firstFailure && firstFailure.status === 'rejected') {
        notifyError(firstFailure.reason, 'Some startup data failed to load');
      }

      await Promise.allSettled([toolsPromise, volumesPromise]);
    })().finally(() => {
      initInFlight = null;
    });

    return initInFlight;
  },

  refreshVolumes: async () => {
    if (!isTauri()) return;
    const seq = ++volumesSeq;
    try {
      const volumes = await api.listVolumes();
      if (seq === volumesSeq) {
        set({ volumes });
      }
    } catch {
      // ignore transient volume poll error
    }
  },

  refreshProfiles: async () => {
    if (!isTauri()) return;
    const seq = ++profilesSeq;
    const [cloudRes, extRes, presetRes] = await Promise.allSettled([
      api.listCloudProfiles(),
      api.listExternalProfiles(),
      api.listImportPresets(),
    ]);
    if (seq !== profilesSeq) return;
    const patch: Partial<SyncStore> = {};
    if (cloudRes.status === 'fulfilled') patch.cloudProfiles = cloudRes.value;
    if (extRes.status === 'fulfilled') patch.externalProfiles = extRes.value;
    if (presetRes.status === 'fulfilled') patch.presets = presetRes.value;
    set(patch);
  },

  refreshJobs: async () => {
    if (!isTauri()) return;
    const seq = ++jobsSeq;
    try {
      const jobs = await api.listJobs(100);
      if (seq === jobsSeq) {
        set({ jobs });
      }
    } catch {
      // ignore
    }
  },

  refreshTools: async () => {
    if (!isTauri()) return;
    const seq = ++toolsSeq;
    try {
      const tools = await api.checkTools();
      if (seq === toolsSeq) {
        set({ tools, toolsChecked: true });
      }
    } catch {
      if (seq === toolsSeq) {
        set({ toolsChecked: true });
      }
    }
  },

  refreshSettings: async () => {
    if (!isTauri()) return;
    const seq = ++settingsSeq;
    try {
      const settings = await api.getSettings();
      if (seq === settingsSeq) {
        set({ settings });
      }
    } catch {
      // ignore
    }
  },

  checkForUpdates: async () => {
    if (!isTauri()) {
      notifyInfo('Updates are checked inside the TruongSync desktop app.');
      return;
    }
    if (get().checkingUpdates || get().installingUpdate) return;
    set({ checkingUpdates: true });
    try {
      const status = await api.checkUpdateFeed();
      set({ updateStatus: status });
      if (status.available && status.version) {
        notifySuccess(`Signed update v${status.version} is ready to install.`);
      } else if (status.notes) {
        notifyInfo(status.notes);
      } else {
        notifyInfo(`TruongSync v${get().bootstrap?.version ?? '0.1.0'} is up to date.`);
      }
    } catch (err) {
      notifyError(err, 'Update check failed');
    } finally {
      set({ checkingUpdates: false });
    }
  },

  installUpdate: async () => {
    const st = get();
    if (!st.updateStatus?.available || st.installingUpdate) return;
    set({
      installingUpdate: true,
      updateProgress: { downloaded: 0, total: null, finished: false },
    });
    try {
      await api.installNativeUpdate();
    } catch (err) {
      notifyError(err, 'Could not install update');
      set({ installingUpdate: false, updateProgress: null });
    }
  },
}));
