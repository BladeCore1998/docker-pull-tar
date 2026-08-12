<template>
  <el-container class="app-container">
    <el-header class="app-header">
      <div class="logo">
        <span class="logo-icon">🚀</span>
        <span class="logo-text">Docker 镜像下载工具</span>
      </div>
      <el-menu mode="horizontal" :default-active="activeMenu" :ellipsis="false" router>
        <el-menu-item index="/">下载</el-menu-item>
        <el-menu-item index="/history">历史</el-menu-item>
        <el-menu-item index="/settings">设置</el-menu-item>
      </el-menu>
    </el-header>
    <el-main class="app-main">
      <router-view />
    </el-main>
  </el-container>
</template>

<script setup lang="ts">
import { computed, onMounted, onUnmounted } from 'vue'
import { useRoute } from 'vue-router'
import { useDownloadStore } from '@/stores/download'

const route = useRoute()
const downloadStore = useDownloadStore()
const activeMenu = computed<string>(() => route.path)

onMounted(() => {
  downloadStore.initialize().catch(error => {
    console.error('Failed to initialize download events:', error)
  })
})

onUnmounted(() => {
  downloadStore.dispose()
})
</script>

<style>
html, body {
  margin: 0;
  padding: 0;
  overflow: hidden;
}
body {
  overflow-y: hidden;
}
.el-main {
  padding: 0 !important;
  overflow-y: hidden;
}
.app-container {
  height: 100vh;
  overflow: hidden;
}
.app-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 0 20px;
  background: #fff;
  border-bottom: 1px solid #e4e7ed;
}
.logo {
  display: flex;
  align-items: center;
  font-size: 18px;
  font-weight: bold;
}
.logo-icon {
  margin-right: 8px;
}
.app-main {
  background: #f5f7fa;
  min-height: 0;
  box-sizing: border-box;
}
</style>
