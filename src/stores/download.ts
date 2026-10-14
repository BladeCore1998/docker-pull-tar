import { defineStore } from 'pinia'
import type { UnlistenFn } from '@tauri-apps/api/event'
import { batchApi, downloadApi } from '@/api/tauri'
import { parseImageInput } from '@/utils/imageParser'
import type {
  DownloadSnapshot,
  BatchSnapshot,
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
  batch: BatchSnapshot | null
  selectedTaskId: string | null
  logs: LogEvent[]
  logsByTaskId: Record<string, LogEvent[]>
  initialized: boolean
  eventUnlisteners: UnlistenFn[]
}

let initialization: Promise<void> | null = null

export const useDownloadStore = defineStore('download', {
  state: (): DownloadState => ({
    snapshot: idleSnapshot(),
    batch: null,
    selectedTaskId: null,
    logs: [],
    logsByTaskId: {},
    initialized: false,
    eventUnlisteners: []
  }),

  getters: {
    taskId: state => state.snapshot.taskId,
    status: state => state.snapshot.status,
    progress: state => state.snapshot.progress,
    lastError: state => state.snapshot.lastError,
    isDownloading: state => ACTIVE_DOWNLOAD_STATUSES.includes(state.snapshot.status) ||
      !!state.batch && ['queued', 'running', 'cancelling'].includes(state.batch.status)
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
        registered.push(await batchApi.onState(batch => this.applyBatchSnapshot(batch)))
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
      this.snapshot = snapshot
      this.logs = snapshot.taskId ? (this.logsByTaskId[snapshot.taskId] || []) : []
      if (this.batch && this.selectedTaskId === snapshot.taskId) {
        const task = this.batch.tasks.find(item => item.taskId === snapshot.taskId)
        if (task) task.download = snapshot
      }
    },

    applyStateEvent(snapshot: DownloadSnapshot): void {
      if (this.batch) {
        const task = this.batch.tasks.find(item => item.taskId === snapshot.taskId)
        if (task) {
          task.download = snapshot
          if (snapshot.taskId === this.selectedTaskId) {
            this.applySnapshot(snapshot)
          }
        }
        return
      }
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
      if (!taskId) return
      if (this.batch) {
        const task = this.batch.tasks.find(item => item.taskId === taskId)
        if (task) task.download.progress = progress
      }
      if (taskId === this.snapshot.taskId) {
        this.snapshot.progress = progress
      }
    },

    applyLogEvent(event: TaskLogEvent): void {
      const { taskId, ...log } = event
      if (!taskId) return
      const existing = this.logsByTaskId[taskId] || []
      const next = [...existing, log].slice(-500)
      this.logsByTaskId[taskId] = next
      if (taskId === this.snapshot.taskId) {
        this.logs = next
      }
    },

    clearLogs(): void {
      if (this.snapshot.taskId) {
        this.logsByTaskId[this.snapshot.taskId] = []
      }
      this.logs = []
    },

    selectTask(taskId: string): void {
      if (!taskId) return
      if (this.batch) {
        const task = this.batch.tasks.find(item => item.taskId === taskId)
        if (!task) return
        this.selectedTaskId = taskId
        this.applySnapshot(task.download)
        return
      }
      if (taskId === this.snapshot.taskId) {
        this.selectedTaskId = taskId
      }
    },

    async startDownload(
      imageInput: string,
      outputDir: string,
      mirror: string,
      arch: string
    ): Promise<DownloadSnapshot> {
      await this.initialize()
      this.batch = null
      this.selectedTaskId = null
      this.logsByTaskId = {}
      this.logs = []
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

    applyBatchSnapshot(batch: BatchSnapshot): void {
      if (this.batch && this.batch.batchId !== batch.batchId) return
      this.batch = batch
      const selected = batch.tasks.find(item => item.taskId === this.selectedTaskId) || batch.tasks[0]
      if (selected) {
        this.selectedTaskId = selected.taskId
        this.applySnapshot(selected.download)
      }
    },

    async startBatch(
      imageInputs: string[],
      outputDir: string,
      mirror: string,
      arch: string,
      maxConcurrentImages = 2
    ): Promise<BatchSnapshot> {
      await this.initialize()
      this.logsByTaskId = {}
      this.logs = []
      const images = imageInputs.map(input => {
        const parsed = parseImageInput(input, mirror)
        if (!parsed) throw new Error(`无效的镜像名称: ${input}`)
        return {
          packageName: parsed.repository,
          tag: parsed.tag,
          arch,
          mirror: parsed.registry,
          outputDir,
          username: null,
          password: null
        }
      })
      const registry = images[0]?.mirror
      if (!registry || images.some(image => image.mirror !== registry)) {
        throw new Error('同一批次必须使用同一个镜像源')
      }
      const batch = await batchApi.start({ images, maxConcurrentImages })
      this.batch = batch
      this.selectedTaskId = batch.tasks[0]?.taskId || null
      const selected = batch.tasks[0]
      if (selected) this.applySnapshot(selected.download)
      return batch
    },

    async refreshBatch(): Promise<BatchSnapshot | null> {
      if (!this.batch) return null
      const batch = await batchApi.status(this.batch.batchId)
      this.batch = batch
      const selected = batch.tasks.find(item => item.taskId === this.selectedTaskId) || batch.tasks[0]
      if (selected) {
        this.selectedTaskId = selected.taskId
        this.applySnapshot(selected.download)
      }
      return batch
    },

    async cancelBatch(): Promise<BatchSnapshot | null> {
      if (!this.batch) return null
      const batch = await batchApi.cancel(this.batch.batchId)
      this.applyBatchSnapshot(batch)
      return batch
    },

    async cancelBatchTask(taskId: string): Promise<BatchSnapshot | null> {
      if (!this.batch) return null
      const batch = await batchApi.cancelTask(this.batch.batchId, taskId)
      this.applyBatchSnapshot(batch)
      return batch
    },

    async retryBatchTask(taskId: string): Promise<BatchSnapshot | null> {
      if (!this.batch) return null
      const batch = await batchApi.retryTask(this.batch.batchId, taskId)
      this.applyBatchSnapshot(batch)
      return batch
    },

    async cancelDownload(): Promise<DownloadSnapshot> {
      if (this.batch && ['queued', 'running', 'cancelling'].includes(this.batch.status)) {
        await this.cancelBatch()
        return this.snapshot
      }
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
