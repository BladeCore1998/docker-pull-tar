import { beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'
import type { BatchSnapshot, DownloadSnapshot } from '@/types/api'
import { useDownloadStore } from './download'

vi.mock('@/api/tauri', () => ({ batchApi: {}, downloadApi: {} }))

function task(taskId: string, repository: string): DownloadSnapshot {
  return {
    taskId,
    image: { registry: 'registry.test', repository, reference: 'latest' },
    status: 'downloading',
    progress: {
      currentLayer: 0, totalLayers: 2, downloadedBytes: 0, totalBytes: 100, speed: 10, layers: []
    },
    startedAt: null,
    finishedAt: null,
    lastError: null
  }
}

function batch(): BatchSnapshot {
  return {
    batchId: 'batch-one',
    status: 'running',
    maxConcurrentImages: 2,
    tasks: [task('one', 'library/one'), task('two', 'library/two')].map(download => ({
      taskId: download.taskId, image: download.image, status: 'running', download
    }))
  }
}

describe('download task progress and logs', () => {
  beforeEach(() => setActivePinia(createPinia()))

  it('retains progress and logs for an unselected task without changing the selected image', () => {
    const store = useDownloadStore()
    store.applyBatchSnapshot(batch())
    store.applyLogEvent({
      taskId: 'one', level: 'info', message: 'one downloading', timestamp: '2026-10-14T00:00:00Z'
    })
    store.applyLogEvent({
      taskId: 'two', level: 'info', message: 'two downloading', timestamp: '2026-10-14T00:00:01Z'
    })
    store.applyProgressEvent({
      ...task('two', 'library/two').progress, taskId: 'two', downloadedBytes: 65
    })
    expect(store.taskId).toBe('one')
    expect(store.logs.map(log => log.message)).toEqual(['one downloading'])

    store.selectTask('two')
    expect(store.progress.downloadedBytes).toBe(65)
    expect(store.logs.map(log => log.message)).toEqual(['two downloading'])
    store.selectTask('one')
    expect(store.logs.map(log => log.message)).toEqual(['one downloading'])
  })

  it('does not let another task preparing or a batch refresh steal the selected task', () => {
    const store = useDownloadStore()
    store.applyBatchSnapshot(batch())
    store.selectTask('two')
    store.applyStateEvent({ ...task('one', 'library/one'), status: 'preparing' })
    store.applyBatchSnapshot(batch())
    expect(store.selectedTaskId).toBe('two')
    expect(store.taskId).toBe('two')
  })

  it('clears only the selected task logs and keeps new events visible', () => {
    const store = useDownloadStore()
    store.applyBatchSnapshot(batch())
    for (const taskId of ['one', 'two']) {
      store.applyLogEvent({ taskId, level: 'info', message: taskId, timestamp: '2026-10-14T00:00:00Z' })
    }
    store.clearLogs()
    expect(store.logs).toEqual([])
    store.selectTask('two')
    expect(store.logs[0].message).toBe('two')
    store.selectTask('one')
    expect(store.logs).toEqual([])
    store.applyLogEvent({
      taskId: 'one', level: 'error', message: 'Download failed: timeout', timestamp: '2026-10-14T00:00:02Z'
    })
    expect(store.logs[0].level).toBe('error')
  })

  it('keeps the last 500 logs per image while continuing to append after the limit', () => {
    const store = useDownloadStore()
    store.applySnapshot(task('one', 'library/one'))
    for (let index = 0; index < 510; index++) {
      store.applyLogEvent({
        taskId: 'one', level: 'info', message: `line ${index}`, timestamp: '2026-10-14T00:00:00Z'
      })
    }
    expect(store.logs).toHaveLength(500)
    expect(store.logs[0].message).toBe('line 10')
    expect(store.logs[store.logs.length - 1]?.message).toBe('line 509')
  })
})
