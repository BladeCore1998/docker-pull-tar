<template>
  <div class="download-progress" v-if="status !== 'idle'">
    <div class="progress-header">
      <span class="progress-title">下载进度</span>
      <span class="progress-status">{{ statusText }}</span>
    </div>
    <div class="progress-layers">
      <div v-for="layer in layers" :key="layer.digest" class="layer-item">
        <div class="layer-header">
          <span class="layer-status-icon">{{ layerStatusText(layer.status) }}</span>
          <span class="layer-name">{{ layer.digest.substring(0, 12) }}</span>
        </div>
        <el-progress
          :percentage="getPercentage(layer)"
          :status="layer.status === 'completed' ? 'success' : ''"
          :stroke-width="10"
        />
        <div class="layer-info">
          <span>{{ formatSize(layer.downloaded) }}/{{ formatSize(layer.total) }}</span>
        </div>
      </div>
    </div>
    <div class="progress-summary">
      <span>速度: {{ formatSpeed(speed) }}</span>
      <span>总计: {{ currentLayer }}/{{ totalLayers }} 层</span>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import { storeToRefs } from 'pinia'
import { useDownloadStore } from '@/stores/download'
import type { DownloadStatus, LayerProgress, LayerStatus } from '@/types/api'

const downloadStore = useDownloadStore()
const { status, progress } = storeToRefs(downloadStore)

const statusText = computed<string>(() => {
  const texts: Record<DownloadStatus, string> = {
    idle: '空闲',
    preparing: '准备中',
    downloading: '下载中',
    verifying: '校验中',
    packing: '打包中',
    completed: '已完成',
    failed: '失败',
    cancelled: '已取消'
  }
  return texts[status.value] || '未知'
})

const layers = computed<LayerProgress[]>(() => progress.value.layers || [])
const currentLayer = computed<number>(() => progress.value.currentLayer || 0)
const totalLayers = computed<number>(() => progress.value.totalLayers || 0)
const speed = computed<number>(() => progress.value.speed || 0)

const formatSize = (bytes: number): string => {
  if (!bytes) return '0B'
  const units: string[] = ['B', 'KB', 'MB', 'GB']
  let size: number = bytes
  for (const unit of units) {
    if (size < 1024) return `${size.toFixed(1)}${unit}`
    size /= 1024
  }
  return `${size.toFixed(1)}TB`
}

const formatSpeed = (bytesPerSec: number): string => {
  return formatSize(bytesPerSec) + '/s'
}

const getPercentage = (layer: LayerProgress): number => {
  if (layer.total === 0) return 0
  return Math.round((layer.downloaded / layer.total) * 100)
}

const layerStatusText = (status: LayerStatus): string => {
  const texts: Record<LayerStatus, string> = {
    waiting: '等待',
    downloading: '下载',
    verifying: '校验',
    completed: '完成',
    failed: '失败',
    cancelled: '取消'
  }
  return texts[status]
}
</script>

<style scoped>
.download-progress {
  background: #fff;
  border-radius: 8px;
  padding: 12px;
  margin-top: 16px;
  border: 1px solid #e4e7ed;
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
.progress-layers {
  max-height: 120px;
  overflow-y: auto;
  min-height: 0;
}
.layer-item {
  margin-bottom: 8px;
}
.layer-header {
  display: flex;
  align-items: center;
  margin-bottom: 4px;
}
.layer-status-icon {
  margin-right: 6px;
}
.layer-name {
  font-size: 12px;
  color: #606266;
}
.layer-info {
  font-size: 12px;
  color: #909399;
  text-align: right;
}
.progress-summary {
  display: flex;
  justify-content: space-between;
  margin-top: 8px;
  font-size: 12px;
  color: #606266;
}
</style>
