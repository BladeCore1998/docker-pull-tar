<template>
  <div class="download-progress" v-if="status !== 'idle'">
    <div class="progress-header">
      <span class="progress-title">&#x4e0b;&#x8f7d;&#x8fdb;&#x5ea6;</span>
      <span class="progress-status">{{ statusText }}</span>
    </div>
    <div class="overall-progress">
      <el-progress
        :percentage="overallPercentage"
        :status="status === 'completed' ? 'success' : ''"
        :stroke-width="12"
      />
      <div class="overall-details">
        <span>{{ overallSizeText }}</span>
        <span>{{ speedText }}</span>
        <span>{{ etaDisplayText }}</span>
        <span>{{ layersText }}</span>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import { storeToRefs } from 'pinia'
import { useDownloadStore } from '@/stores/download'
import type { DownloadStatus } from '@/types/api'

const downloadStore = useDownloadStore()
const { status, progress } = storeToRefs(downloadStore)

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
  return texts[status.value]
})

const currentLayer = computed<number>(() => progress.value.currentLayer || 0)
const totalLayers = computed<number>(() => progress.value.totalLayers || 0)
const speed = computed<number>(() => progress.value.speed || 0)
const downloadedBytes = computed<number>(() => progress.value.downloadedBytes || 0)
const totalBytes = computed<number>(() => progress.value.totalBytes || 0)
const overallPercentage = computed<number>(() => {
  if (totalBytes.value === 0) return status.value === 'completed' ? 100 : 0
  return Math.min(100, Math.round((downloadedBytes.value / totalBytes.value) * 100))
})
const etaText = computed<string>(() => {
  const remainingBytes = totalBytes.value - downloadedBytes.value
  if (status.value === 'completed' || remainingBytes <= 0) return '0s'
  if (status.value !== 'downloading' || speed.value <= 0) return '--'
  return formatDuration(Math.ceil(remainingBytes / speed.value))
})
const overallSizeText = computed<string>(() =>
  `${formatSize(downloadedBytes.value)}/${formatSize(totalBytes.value)} (${overallPercentage.value}%)`
)
const speedText = computed<string>(() => `\u901f\u5ea6: ${formatSpeed(speed.value)}`)
const etaDisplayText = computed<string>(() => `\u9884\u8ba1\u5269\u4f59: ${etaText.value}`)
const layersText = computed<string>(() => `\u603b\u8ba1: ${currentLayer.value}/${totalLayers.value} \u5c42`)

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
  background: #fff;
  border: 1px solid #e4e7ed;
  border-radius: 8px;
  margin-top: 16px;
  padding: 12px;
}

.progress-header {
  display: flex;
  justify-content: space-between;
  margin-bottom: 12px;
}

.progress-title {
  font-weight: bold;
}

.progress-status {
  color: #409eff;
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
</style>
