import { invoke } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import type {
  AppSettings,
  BatchSnapshot,
  BatchStartRequest,
  CommandResult,
  DownloadSnapshot,
  DownloadStartRequest,
  HistoryItem,
  Mirror,
  MirrorInput,
  MirrorPatch,
  SettingsPatch,
  TaskLogEvent,
  TaskProgressEvent
} from '@/types/api'

type EventHandler<T> = (payload: T) => void

function listenTo<T>(event: string, handler: EventHandler<T>): Promise<UnlistenFn> {
  return listen<T>(event, ({ payload }) => handler(payload))
}

export const downloadApi = {
  start: (request: DownloadStartRequest): Promise<DownloadSnapshot> =>
    invoke('download_start', { request }),
  cancel: (): Promise<DownloadSnapshot> => invoke('download_cancel'),
  status: (): Promise<DownloadSnapshot> => invoke('download_status'),
  onState: (handler: EventHandler<DownloadSnapshot>): Promise<UnlistenFn> =>
    listenTo('download://state', handler),
  onProgress: (handler: EventHandler<TaskProgressEvent>): Promise<UnlistenFn> =>
    listenTo('download://progress', handler),
  onLog: (handler: EventHandler<TaskLogEvent>): Promise<UnlistenFn> =>
    listenTo('download://log', handler)
}

export const batchApi = {
  start: (request: BatchStartRequest): Promise<BatchSnapshot> =>
    invoke('batch_start', { request }),
  status: (batchId: string): Promise<BatchSnapshot> =>
    invoke('batch_status', { batchId }),
  cancel: (batchId: string): Promise<BatchSnapshot> =>
    invoke('batch_cancel', { batchId }),
  cancelTask: (batchId: string, taskId: string): Promise<BatchSnapshot> =>
    invoke('batch_task_cancel', { batchId, taskId }),
  retryTask: (batchId: string, taskId: string): Promise<BatchSnapshot> =>
    invoke('batch_task_retry', { batchId, taskId }),
  onState: (handler: EventHandler<BatchSnapshot>): Promise<UnlistenFn> =>
    listenTo('batch://state', handler)
}

export const settingsApi = {
  get: (): Promise<AppSettings> => invoke('settings_get'),
  update: (patch: SettingsPatch): Promise<AppSettings> =>
    invoke('settings_update', { patch })
}

export const mirrorsApi = {
  list: (): Promise<Mirror[]> => invoke('mirrors_list'),
  add: (input: MirrorInput): Promise<Mirror> => invoke('mirror_add', { input }),
  update: (id: string, patch: MirrorPatch): Promise<Mirror> =>
    invoke('mirror_update', { id, patch }),
  delete: (id: string): Promise<CommandResult> => invoke('mirror_delete', { id }),
  setDefault: (id: string): Promise<AppSettings> =>
    invoke('mirror_set_default', { id })
}

export const historyApi = {
  list: (): Promise<HistoryItem[]> => invoke('history_list'),
  delete: (id: string): Promise<CommandResult> => invoke('history_delete', { id }),
  clear: (): Promise<CommandResult> => invoke('history_clear')
}

export const systemApi = {
  selectDirectory: (initialDirectory?: string): Promise<string | null> =>
    invoke('dialog_select_directory', { initialDirectory: initialDirectory || null }),
  openParent: (path: string): Promise<CommandResult> =>
    invoke('path_open_parent', { path })
}
