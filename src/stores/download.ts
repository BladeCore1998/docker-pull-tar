import { defineStore } from 'pinia'
import type { UnlistenFn } from '@tauri-apps/api/event'
import { downloadApi } from '@/api/tauri'
import { parseImageInput } from '@/utils/imageParser'
import type {
  DownloadSnapshot,
  DownloadStatus,
  LogEvent,
  TaskLogEvent,
  TaskProgressEvent
} from '@/types/api'

export const ACTIVE_DOWNLOAD_STATUSES: DownloadStatus[] = [
  'preparing',
  'downloading',
  'verifying',
  'packing'
]

const TERMINAL_DOWNLOAD_STATUSES: DownloadStatus[] = [
  'idle',
  'completed',
  'failed',
  'cancelled'
]

function idleSnapshot(): DownloadSnapshot {
  return {
    taskId: '',
    image: {
      registry: '',
      repository: '',
      reference: ''
    },
    status: 'idle',
    progress: {
      currentLayer: 0,
      totalLayers: 0,
      downloadedBytes: 0,
      totalBytes: 0,
      speed: 0,
      layers: []
    },
    startedAt: null,
    finishedAt: null,
    lastError: null
  }
}

interface DownloadState {
  snapshot: DownloadSnapshot
  logs: LogEvent[]
  initialized: boolean
  eventUnlisteners: UnlistenFn[]
}

let initialization: Promise<void> | null = null

export const useDownloadStore = defineStore('download', {
  state: (): DownloadState => ({
    snapshot: idleSnapshot(),
    logs: [],
    initialized: false,
    eventUnlisteners: []
  }),

  getters: {
    taskId: state => state.snapshot.taskId,
    status: state => state.snapshot.status,
    progress: state => state.snapshot.progress,
    lastError: state => state.snapshot.lastError,
    isDownloading: state => ACTIVE_DOWNLOAD_STATUSES.includes(state.snapshot.status)
  },

  actions: {
    async initialize(): Promise<void> {
      if (this.initialized) return
      if (initialization) return initialization

      initialization = this.registerEventListeners()
      try {
        await initialization
      } finally {
        initialization = null
      }
    },

    async registerEventListeners(): Promise<void> {
      const registered: UnlistenFn[] = []
      try {
        registered.push(await downloadApi.onState(snapshot => this.applyStateEvent(snapshot)))
        registered.push(await downloadApi.onProgress(event => this.applyProgressEvent(event)))
        registered.push(await downloadApi.onLog(event => this.applyLogEvent(event)))
        this.eventUnlisteners = registered

        const snapshot = await downloadApi.status()
        this.applySnapshot(snapshot)
        this.initialized = true
      } catch (error) {
        registered.forEach(unlisten => unlisten())
        this.eventUnlisteners = []
        throw error
      }
    },

    dispose(): void {
      this.eventUnlisteners.forEach(unlisten => unlisten())
      this.eventUnlisteners = []
      this.initialized = false
      initialization = null
    },

    applySnapshot(snapshot: DownloadSnapshot): void {
      if (snapshot.taskId && snapshot.taskId !== this.snapshot.taskId) {
        this.logs = []
      }
      this.snapshot = snapshot
    },

    applyStateEvent(snapshot: DownloadSnapshot): void {
      const isCurrentTask = snapshot.taskId === this.snapshot.taskId
      const canAdoptTask = !this.snapshot.taskId || (
        snapshot.status === 'preparing' &&
        TERMINAL_DOWNLOAD_STATUSES.includes(this.snapshot.status)
      )
      if (isCurrentTask || canAdoptTask) {
        this.applySnapshot(snapshot)
      }
    },

    applyProgressEvent(event: TaskProgressEvent): void {
      const { taskId, ...progress } = event
      if (!taskId || taskId !== this.snapshot.taskId) return
      this.snapshot.progress = progress
    },

    applyLogEvent(event: TaskLogEvent): void {
      const { taskId, ...log } = event
      if (!taskId || taskId !== this.snapshot.taskId) return
      this.addLog(log)
    },

    addLog(log: LogEvent): void {
      this.logs.push(log)
      if (this.logs.length > 500) {
        this.logs = this.logs.slice(-500)
      }
    },

    clearLogs(): void {
      this.logs = []
    },

    async startDownload(
      imageInput: string,
      outputDir: string,
      mirror: string,
      arch: string
    ): Promise<DownloadSnapshot> {
      await this.initialize()
      const parsed = parseImageInput(imageInput, mirror)
      if (!parsed) {
        throw new Error('请输入有效的镜像名称')
      }

      const snapshot = await downloadApi.start({
        packageName: parsed.repository,
        tag: parsed.tag,
        arch,
        mirror: parsed.registry,
        outputDir,
        username: null,
        password: null
      })
      this.applySnapshot(snapshot)
      return snapshot
    },

    async cancelDownload(): Promise<DownloadSnapshot> {
      const snapshot = await downloadApi.cancel()
      this.applySnapshot(snapshot)
      return snapshot
    },

    async refreshStatus(): Promise<DownloadSnapshot> {
      const snapshot = await downloadApi.status()
      this.applySnapshot(snapshot)
      return snapshot
    }
  }
})
