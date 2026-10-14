export interface CommandError {
  code: string
  message: string
  retryable: boolean
  details?: Record<string, string>
}

export interface CommandResult {
  success: boolean
}

export interface AppSettings {
  defaultOutputDir: string
  defaultArch: string
  defaultMirror: string
  downloadWorkers: number
  lastPackageName: string | null
}

export interface SettingsPatch {
  defaultOutputDir?: string
  defaultArch?: string
  defaultMirror?: string
  downloadWorkers?: number
  lastPackageName?: string | null
}

export interface Mirror {
  id: string
  name: string
  registry: string
  isDefault: boolean
  username: string | null
  password: string | null
}

export interface MirrorInput {
  name: string
  registry: string
  isDefault: boolean
  username: string | null
  password: string | null
}

export interface MirrorPatch {
  name?: string
  registry?: string
  isDefault?: boolean
  username?: string | null
  password?: string | null
}

export interface DownloadStartRequest {
  packageName: string
  tag: string
  arch: string
  mirror: string
  outputDir: string
  username: string | null
  password: string | null
}

export interface BatchStartRequest {
  images: DownloadStartRequest[]
  maxConcurrentImages: number
}

export type BatchStatus =
  | 'queued'
  | 'running'
  | 'cancelling'
  | 'completed'
  | 'partial_failed'
  | 'failed'
  | 'cancelled'

export type BatchTaskStatus = 'queued' | 'running' | 'completed' | 'failed' | 'cancelled'

export type DownloadStatus =
  | 'idle'
  | 'preparing'
  | 'downloading'
  | 'verifying'
  | 'packing'
  | 'completed'
  | 'failed'
  | 'cancelled'

export type LayerStatus =
  | 'waiting'
  | 'downloading'
  | 'verifying'
  | 'completed'
  | 'failed'
  | 'cancelled'

export interface LayerProgress {
  digest: string
  size: number
  status: LayerStatus
  downloaded: number
  total: number
}

export interface DownloadProgress {
  currentLayer: number
  totalLayers: number
  downloadedBytes: number
  totalBytes: number
  speed: number
  layers: LayerProgress[]
}

export interface ImageReference {
  registry: string
  repository: string
  reference: string
}

export interface DownloadSnapshot {
  taskId: string
  image: ImageReference
  status: DownloadStatus
  progress: DownloadProgress
  startedAt: string | null
  finishedAt: string | null
  lastError: string | null
}

export interface BatchTaskSnapshot {
  taskId: string
  image: ImageReference
  status: BatchTaskStatus
  download: DownloadSnapshot
}

export interface BatchSnapshot {
  batchId: string
  status: BatchStatus
  maxConcurrentImages: number
  tasks: BatchTaskSnapshot[]
}

export interface TaskProgressEvent extends DownloadProgress {
  taskId: string
}

export interface LogEvent {
  level: string
  message: string
  timestamp: string
}

export interface TaskLogEvent extends LogEvent {
  taskId: string
}

export interface HistoryItem {
  id: string
  packageName: string
  tag: string
  arch: string
  mirror: string
  fullImage: string
  outputPath: string | null
  size: string | null
  status: DownloadStatus
  downloadedAt: string | null
  error: string | null
}
