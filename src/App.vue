<template>
  <el-container class="app-container">
    <el-header class="app-header">
      <div class="logo">
        <span class="logo-mark">D</span>
        <div class="logo-copy">
          <span class="logo-text">Docker Pull Tar</span>
        </div>
      </div>
      <el-menu class="app-nav" mode="horizontal" :default-active="activeMenu" :ellipsis="false" router>
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
:root {
  --el-color-primary: #6155d9;
  --el-color-primary-light-3: #8a80e4;
  --el-color-primary-light-5: #b0a9ec;
  --el-color-primary-light-7: #d1ccf4;
  --el-color-primary-light-8: #dfdbf8;
  --el-color-primary-light-9: #efedfc;
  --el-border-radius-base: 9px;
  --el-font-family: "Segoe UI", "Microsoft YaHei", sans-serif;
}
html, body {
  margin: 0;
  padding: 0;
  overflow: hidden;
  color: #172033;
  background: #eef2ff;
  font-family: "Segoe UI", "Microsoft YaHei", sans-serif;
}
body {
  overflow-y: hidden;
}
.el-main {
  padding: 0 !important;
  overflow-y: auto;
}
.app-container {
  height: 100vh;
  overflow: hidden;
}
.app-header {
  height: 60px;
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 0 28px;
  background: rgba(255, 255, 255, 0.82);
  border-bottom: 0;
  box-shadow: 0 8px 24px rgba(75, 85, 180, 0.05);
  backdrop-filter: blur(18px);
  position: relative;
  z-index: 2;
}
.logo {
  display: flex;
  align-items: center;
  min-width: 220px;
}
.logo-mark {
  align-items: center;
  background: linear-gradient(135deg, #6556f4 0%, #18b6d9 100%);
  border-radius: 13px;
  box-shadow: 0 5px 12px rgba(101, 86, 244, 0.16);
  color: #fff;
  display: inline-flex;
  font-size: 18px;
  font-weight: 800;
  height: 34px;
  justify-content: center;
  margin-right: 10px;
  width: 34px;
}
.logo-copy {
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.logo-text {
  color: #172033;
  font-size: 16px;
  font-weight: 650;
  letter-spacing: -0.01em;
}
.app-nav {
  background: transparent;
  border-bottom: 0;
}
.app-nav .el-menu-item {
  border: 0 !important;
  border-radius: 10px;
  color: #7b8499;
  height: 38px;
  line-height: 38px;
  margin-left: 4px;
  padding: 0 14px;
}
.app-nav .el-menu-item:hover {
  background: rgba(101, 86, 244, 0.08);
  color: #4f46c6;
}
.app-nav .el-menu-item.is-active {
  background: linear-gradient(135deg, rgba(101, 86, 244, 0.12), rgba(24, 182, 217, 0.1));
  color: #4f46c6;
  font-weight: 650;
}
.app-main {
  background:
    radial-gradient(circle at 8% 0%, rgba(130, 115, 255, 0.16), transparent 28%),
    radial-gradient(circle at 96% 12%, rgba(34, 211, 238, 0.12), transparent 26%),
    linear-gradient(135deg, #f8f9ff 0%, #eef3ff 54%, #f7fbff 100%);
  min-height: 0;
  box-sizing: border-box;
}
.el-input__wrapper,
.el-select__wrapper,
.el-textarea__inner,
.el-input-number,
.el-input-group__append {
  background: #f3f5f9;
  border: 0;
  box-shadow: none !important;
}
.el-input__wrapper,
.el-select__wrapper {
  min-height: 34px;
}
.el-input__wrapper.is-focus,
.el-select__wrapper.is-focused,
.el-textarea__inner:focus {
  background: #efedfc;
  box-shadow: 0 0 0 2px #b0a9ec !important;
}
.el-button {
  border: 0;
  background: #f0f2f7;
}
.el-button.is-link,
.el-button.is-text {
  background: transparent;
}
.el-button--primary {
  background: var(--el-color-primary);
  color: #fff;
}
.el-button--primary.is-plain {
  background: #efedfc;
  color: #6155d9;
}
.el-button--danger {
  background: #fdefef;
  color: #c84657;
}
.el-button--warning {
  background: #fff4e4;
  color: #b07318;
}
.el-card {
  border: 0;
  border-radius: 16px;
}
.el-card__header {
  border: 0;
}
.el-tag {
  border: 0;
}
.el-dialog {
  border: 0;
  border-radius: 18px;
  max-width: calc(100vw - 24px);
  padding: 22px;
}
.el-dialog__body {
  max-height: 75vh;
  overflow-y: auto;
}
.el-dialog__title {
  color: #263249;
  font-size: 17px;
  font-weight: 600;
}
.el-dialog__header,
.el-dialog__footer {
  padding-bottom: 16px;
}
.el-dialog__footer {
  padding-top: 16px;
  padding-bottom: 0;
}
.el-table {
  --el-table-border-color: transparent;
  --el-table-header-bg-color: #f6f8fc;
}
.el-divider {
  border: 0;
}
.el-divider__text {
  color: #7b8499;
}
.el-select-dropdown.el-popper {
  border: 0;
}
.el-button:focus-visible,
.el-menu-item:focus-visible {
  outline: 2px solid #6155d9;
  outline-offset: 2px;
}
@media (max-width: 720px) {
  .app-header {
    padding: 0 12px;
  }
  .logo {
    min-width: 0;
  }
  .logo-mark {
    border-radius: 11px;
    height: 34px;
    margin-right: 8px;
    width: 34px;
  }
  .logo-text {
    font-size: 14px;
  }
  .app-nav {
    max-width: 190px;
    overflow-x: auto;
  }
  .app-nav .el-menu-item {
    font-size: 12px;
    padding: 0 9px;
  }
}
</style>
