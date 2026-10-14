<template>
  <div v-if="snapshot.taskId || status !== 'idle'" class="download-progress" :class="{ 'is-embedded': embedded }">
    <div class="progress-header">
      <div class="progress-heading">
        <span class="progress-title">总体进度</span>
        <span v-if="imageText && !downloadStore.batch" class="progress-image" :title="imageText">{{ imageText }}</span>
      </div>
      <span class="progress-status">{{ statusText }}</span>
    </div>
    <div class="overall-progress">
      <el-progress
        :percentage="overallPercentage"
        :status="status === 'completed' ? 'success' : status === 'failed' ? 'exception' : undefined"
        :stroke-width="12"
      />
      <div class="overall-details">
        <span>{{ overallSizeText }}</span>
        <span>{{ speedText }}</span>
        <span>{{ etaDisplayText }}</span>
        <span>{{ layersText }}</span>
      </div>
    </div>
    <div v-if="snapshot.lastError" class="download-error" role="alert">
      {{ snapshot.lastError }}
    </div>
    <div v-if="progress.layers.length > 0" class="layer-list">
      <div class="layer-list-header">
        <span>层级进度</span>
        <span>{{ progress.layers.length }} 层</span>
      </div>
      <div v-for="layer in progress.layers" :key="layer.digest" class="layer-row">
        <div class="layer-meta">
          <span class="layer-digest" :title="layer.digest">{{ shortDigest(layer.digest) }}</span>
          <span class="layer-status">{{ layerStatusText(layer.status) }}</span>
        </div>
        <el-progress
          :percentage="layerPercentage(layer)"
          :status="layerProgressStatus(layer.status)"
          :stroke-width="8"
          :show-text="false"
        />
        <span class="layer-size">{{ formatSize(layer.downloaded) }}/{{ formatSize(layer.total) }}</span>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import { storeToRefs } from 'pinia'
import { useDownloadStore } from '@/stores/download'
import type { DownloadStatus, LayerProgress, LayerStatus } from '@/types/api'

withDefaults(defineProps<{ embedded?: boolean }>(), {
  embedded: false
})
const downloadStore = useDownloadStore()
const { snapshot, status, progress } = storeToRefs(downloadStore)

const imageText = computed(() => {
  const image = snapshot.value.image
  if (!image.repository) return ''
  return `${image.registry}/${image.repository}${image.reference.startsWith('sha256:') ? '@' : ':'}${image.reference}`
})

const statusText = computed<string>(() => {
  const texts: Record<DownloadStatus, string> = {
    idle: '\u7a7a\u95f2',
    preparing: '\u51c6\u5907\u4e2d',
    downloading: '\u4e0b\u8f7d\u4e2d',
    verifying: '\u6821\u9a8c\u4e2d',
    packing: '\u6253\u5305\u4e2d',
    completed: '\u5df2\u5b8c\u6210',
    failed: '\u5931\u8d25',
    cancelled: '\u5df2\u53d6\u6d88'
  }
  if (status.value === 'idle' && downloadStore.batch?.tasks.find(task => task.taskId === snapshot.value.taskId)?.status === 'queued') {
    return '等待中'
  }
  return texts[status.value]
})

const currentLayer = computed<number>(() => progress.value.currentLayer || 0)
const totalLayers = computed<number>(() => progress.value.totalLayers || 0)
const speed = computed<number>(() => status.value === 'downloading' ? (progress.value.speed || 0) : 0)
const downloadedBytes = computed<number>(() => progress.value.downloadedBytes || 0)
const totalBytes = computed<number>(() => progress.value.totalBytes || 0)
const overallPercentage = computed<number>(() => {
  if (status.value === 'completed') return 100
  if (totalBytes.value === 0) return 0
  return Math.min(100, Math.round((downloadedBytes.value / totalBytes.value) * 100))
})
const etaText = computed<string>(() => {
  const remainingBytes = totalBytes.value - downloadedBytes.value
  if (status.value === 'completed') return '0s'
  if (status.value !== 'downloading' || speed.value <= 0) return '--'
  if (remainingBytes <= 0) return '--'
  return formatDuration(Math.ceil(remainingBytes / speed.value))
})
const overallSizeText = computed<string>(() =>
  `${formatSize(downloadedBytes.value)}/${formatSize(totalBytes.value)} (${overallPercentage.value}%)`
)
const speedText = computed<string>(() => `\u901f\u5ea6: ${formatSpeed(speed.value)}`)
const etaDisplayText = computed<string>(() => `\u9884\u8ba1\u5269\u4f59: ${etaText.value}`)
const layersText = computed<string>(() => `\u603b\u8ba1: ${currentLayer.value}/${totalLayers.value} \u5c42`)

const layerStatusText = (layerStatus: LayerStatus): string => {
  const texts: Record<LayerStatus, string> = {
    waiting: '等待中',
    downloading: '下载中',
    verifying: '校验中',
    completed: '已完成',
    failed: '失败',
    cancelled: '已取消'
  }
  return texts[layerStatus]
}

const layerPercentage = (layer: LayerProgress): number => {
  if (layer.status === 'completed') return 100
  if (!layer.total) return 0
  return Math.min(100, Math.round((layer.downloaded / layer.total) * 100))
}

const layerProgressStatus = (layerStatus: LayerStatus): 'success' | 'exception' | undefined => {
  if (layerStatus === 'completed') return 'success'
  if (layerStatus === 'failed' || layerStatus === 'cancelled') return 'exception'
  return undefined
}

const shortDigest = (digest: string): string => {
  if (digest.length <= 24) return digest
  return `${digest.slice(0, 18)}…${digest.slice(-6)}`
}

const formatSize = (bytes: number): string => {
  if (!bytes) return '0B'
  const units = ['B', 'KB', 'MB', 'GB']
  let size = bytes
  for (const unit of units) {
    if (size < 1024) return `${size.toFixed(1)}${unit}`
    size /= 1024
  }
  return `${size.toFixed(1)}TB`
}

const formatSpeed = (bytesPerSec: number): string => `${formatSize(bytesPerSec)}/s`

const formatDuration = (seconds: number): string => {
  const hours = Math.floor(seconds / 3600)
  const minutes = Math.floor((seconds % 3600) / 60)
  const remainingSeconds = seconds % 60
  if (hours > 0) return `${hours}h ${minutes.toString().padStart(2, '0')}m ${remainingSeconds.toString().padStart(2, '0')}s`
  if (minutes > 0) return `${minutes}m ${remainingSeconds.toString().padStart(2, '0')}s`
  return `${remainingSeconds}s`
}
</script>

<style scoped>
.download-progress {
  background: transparent;
  border: 0;
  border-radius: 8px;
  margin-top: 16px;
  padding: 12px;
}

.download-progress.is-embedded {
  border: 0;
  margin-top: 0;
  padding: 0;
}

.progress-header {
  align-items: center;
  display: flex;
  gap: 10px;
  justify-content: space-between;
  margin-bottom: 12px;
}

.progress-title {
  flex: 0 0 auto;
  font-size: 13px;
  font-weight: 600;
}

.progress-heading {
  display: flex;
  flex: 1;
  gap: 8px;
  min-width: 0;
}

.progress-image {
  color: #606266;
  font-size: 12px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.progress-status {
  color: #6155d9;
  font-size: 12px;
  flex: 0 0 auto;
}

.overall-details {
  color: #606266;
  display: flex;
  flex-wrap: wrap;
  font-size: 12px;
  gap: 4px 12px;
  justify-content: space-between;
  margin-top: 6px;
}
.download-error {
  background: #fdefef;
  border-radius: 9px;
  color: #c84657;
  font-size: 12px;
  margin-top: 12px;
  overflow-wrap: anywhere;
  padding: 10px;
}

.layer-list {
  margin-top: 12px;
  max-height: 180px;
  overflow-y: auto;
  padding-top: 10px;
}

.layer-list-header,
.layer-meta {
  align-items: center;
  display: flex;
  justify-content: space-between;
}

.layer-list-header {
  color: #606266;
  font-size: 12px;
  margin-bottom: 8px;
}

.layer-row {
  display: grid;
  gap: 4px 8px;
  grid-template-columns: minmax(0, 1fr) 80px;
  margin-bottom: 8px;
}

.layer-meta {
  grid-column: 1 / -1;
  font-size: 12px;
}

.layer-digest {
  color: #606266;
  font-family: monospace;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.layer-status {
  color: #909399;
}

.layer-size {
  color: #909399;
  font-size: 11px;
  text-align: right;
}
</style>
